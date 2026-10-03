//! Graph IR of the graph tier: SSA value nodes in basic blocks, each block
//! ending in one control node, with deopt frame states attached to the nodes
//! that can leave optimized code.
//!
//! # Contents
//! - [`NodeId`], [`BlockId`], [`FrameStateId`] — dense arena indices.
//! - [`Repr`] — the machine representation of a node's result.
//! - [`Kind`] — every node operation, value and control alike.
//! - [`Node`], [`Block`], [`FrameState`], [`Graph`] — the arena.
//! - [`InlineCaller`] / [`InlinedBody`] — how an inlined body's frames and
//!   nodes name the call they run for.
//! - [`Properties`] / [`Kind::properties`] — what a node may do (call,
//!   deopt eagerly or lazily, throw, read or write the heap).
//! - [`Constraints`] / [`Kind::constraints`] — where the register allocator
//!   must put inputs, the result, and how many temporaries a node needs.
//!
//! # Invariants
//! - Constants live outside blocks and are materialized at each use; every
//!   other value node is placed in exactly one block.
//! - Every block ends in exactly one control node; phis precede the body.
//! - A phi has one input per predecessor, in predecessor order. A loop
//!   header's last predecessor is its back edge.
//! - A node with [`Properties::eager_deopt`] has an eager frame state that
//!   resumes *before* its bytecode instruction; a node with
//!   [`Properties::lazy_deopt`] has a lazy frame state that resumes *after*
//!   it, with the node itself bound to the instruction's result register.
//! - A call ([`Properties::call`]) clobbers every allocatable register; the
//!   allocator spills every live value before it.
//! - A frame state of an inlined body links to its caller's state after the
//!   call; the values of the whole chain stay live wherever it is read.
//!
//! # See also
//! - [`super::builder`] — builds the graph from bytecode and feedback.
//! - [`super::regalloc`] — assigns locations from [`Constraints`].

use otter_vm::jit::{JitBodyGuard, JitElementRepr, JitGuardWidth, JitHoleBitmap};
use smallvec::SmallVec;

/// Dense node index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(pub(crate) u32);

/// Dense block index; block order is emission order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct BlockId(pub(crate) u32);

/// Dense frame-state index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct FrameStateId(pub(crate) u32);

/// Machine representation of a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Repr {
    /// The node produces no value.
    None,
    /// A full tagged `Value` word; a GC root while live.
    Tagged,
    /// A signed 32-bit integer in the low word of a general register.
    Int32,
    /// An unboxed double in a floating-point register.
    Float64,
    /// A raw machine word that is never a GC root: an address inside a
    /// non-moving body, a length, a bit pattern.
    Word,
}

impl Repr {
    pub(crate) fn is_float(self) -> bool {
        self == Self::Float64
    }
}

/// A condition on two int32 or two float64 operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Condition {
    Equal,
    NotEqual,
    Less,
    LessEqual,
    Greater,
    GreaterEqual,
}

impl Condition {
    pub(crate) fn negate(self) -> Self {
        match self {
            Self::Equal => Self::NotEqual,
            Self::NotEqual => Self::Equal,
            Self::Less => Self::GreaterEqual,
            Self::LessEqual => Self::Greater,
            Self::Greater => Self::LessEqual,
            Self::GreaterEqual => Self::Less,
        }
    }
}

/// Why an eager deopt leaves optimized code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DeoptReason {
    /// A value did not have the speculated type.
    WrongType,
    /// An object did not have one of the speculated shapes.
    WrongShape,
    /// A value was not the speculated constant.
    WrongValue,
    /// Int32 arithmetic overflowed.
    Overflow,
    /// An int32 result would have been negative zero.
    MinusZero,
    /// An index was outside the speculated bounds.
    OutOfBounds,
    /// An element key was not an integral Number in the int32 range.
    InvalidIndex,
    /// A double did not convert exactly to int32.
    LostPrecision,
    /// The site had never executed when this code was compiled.
    InsufficientFeedback,
    /// The operation has no optimized form and resumes in the interpreter
    /// (suspension, irreducible entry).
    Unsupported,
}

/// One branch condition of a [`Kind::Branch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BranchKind {
    /// `ToBoolean(input0)`.
    Truthy,
    /// `input0 cond input1` on int32 operands.
    Int32(Condition),
    /// `input0 cond input1` on float64 operands; unordered is false.
    Float64(Condition),
    /// `input0` and `input1` are the same tagged word.
    TaggedEqual,
    /// `input0` is `undefined` or `null`.
    Nullish,
    /// The word `input0` equals this constant.
    WordEqual(u32),
}

/// Every graph operation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Kind {
    // ---- Constants (outside blocks) ----
    /// A tagged immediate (never a heap address).
    ConstTagged(u64),
    ConstInt32(i32),
    /// Float64 by bit pattern.
    ConstFloat64(u64),

    // ---- Frame ----
    /// Interpreter register `0` at function entry or OSR entry, read from
    /// the frame window.
    InitialRegister(u16),
    /// The frame's `this` binding from the record.
    LoadThis,
    /// The callee (SELF) from the record.
    LoadClosure,
    /// The primitive string constant of the `LoadString` at this byte PC,
    /// read from its isolate-owned cell.
    LoadStringConstant(u32),
    /// `new.target` from the record.
    LoadNewTarget,
    /// Read a window-resident register.
    LoadWindow(u16),
    /// Write a window-resident register.
    StoreWindow(u16),
    /// A merge of one value per predecessor.
    Phi,

    // ---- Int32 arithmetic ----
    /// Checked add; eager deopt on overflow.
    Int32Add,
    Int32Sub,
    Int32Mul,
    /// Checked exact division; deopt on a remainder, zero divisor, overflow
    /// or negative zero.
    Int32Div,
    /// Checked remainder; deopt on zero divisor or negative zero.
    Int32Mod,
    /// Checked negation; deopt on zero and on `i32::MIN`.
    Int32Negate,
    Int32BitAnd,
    Int32BitOr,
    Int32BitXor,
    Int32BitNot,
    Int32ShiftLeft,
    Int32ShiftRight,
    /// Logical shift right whose result must fit int32; deopts otherwise.
    Int32ShiftRightLogical,
    /// Logical shift right of int32 operands as a Float64: the unsigned
    /// result needs no check.
    Uint32ShiftRightToFloat64,
    /// Int32 compare producing a tagged boolean.
    Int32Compare(Condition),

    // ---- Float64 arithmetic ----
    Float64Add,
    Float64Sub,
    Float64Mul,
    Float64Div,
    Float64Negate,
    /// `fmod` through a leaf call.
    Float64Mod,
    /// Float64 compare producing a tagged boolean; unordered is false.
    Float64Compare(Condition),

    // ---- Conversions ----
    /// Tagged int32 to Int32; eager deopt for anything else.
    CheckedTaggedToInt32,
    /// Tagged number (int32 or double) to Float64; eager deopt otherwise.
    CheckedTaggedToFloat64,
    /// Box an int32.
    Int32ToTagged,
    /// Box a double in the canonical number encoding (integral int32 values
    /// other than -0 become int32 words).
    Float64ToTagged,
    Int32ToFloat64,
    /// ECMAScript `ToInt32` of a double.
    TruncateFloat64ToInt32,
    /// Exact double to int32; eager deopt when not exact or -0.
    CheckedFloat64ToInt32,
    /// An element index from a tagged Number: an int32, or a double holding
    /// an integer in the int32 range (`-0` is `0`). Eager deopt otherwise.
    CheckedTaggedToIndex,
    /// An element index from a double holding an integer in the int32 range
    /// (`-0` is `0`). Eager deopt otherwise.
    CheckedFloat64ToIndex,
    /// Strict equality of two tagged words producing a tagged boolean.
    TaggedEqual,
    /// `input0 === input1` (or `!==` when `negate`) on any two tagged values,
    /// producing a tagged boolean: numbers by value, strings and BigInts by
    /// content through a leaf probe, everything else by identity.
    StrictEqual {
        negate: bool,
    },
    /// `ToBoolean` producing a tagged boolean.
    ToBoolean,
    /// `!ToBoolean` producing a tagged boolean.
    LogicalNot,

    // ---- Checks ----
    /// Eager deopt unless input0 is a heap cell.
    CheckHeapObject,
    /// Eager deopt unless input0 is a Number.
    CheckNumber,
    /// Eager deopt unless input0 is an ordinary object with one of the
    /// shape handles (compressed offsets) and no object-local state
    /// overriding its slots; `writable` also requires that the object is
    /// not a prototype (whose slot writes must invalidate dependents).
    CheckShapes {
        shapes: SmallVec<[u32; 4]>,
        writable: bool,
    },
    /// Eager deopt unless input0 is exactly this tagged word.
    CheckValue(u64),
    /// Eager deopt unless the int32 index input0 lies in `0..input1`, where
    /// input1 is a `Word` element count.
    CheckBounds,
    /// Eager deopt unless input0 is a heap cell of this body type whose
    /// instance guards hold, whose storage kind is one of the two numeric
    /// kinds for numeric storage that may have holes, and whose cached
    /// element base is valid for a typed view: the indexed storage then has
    /// the access's layout.
    CheckElements {
        type_tag: u8,
        guards: [Option<JitBodyGuard>; 2],
        holes: Option<JitHoleBitmap>,
        /// For a typed view: the byte offset of its cached element base,
        /// which must be non-null while no buffer was ever detached.
        cached_base: Option<u32>,
    },
    /// Eager deopt if the boxed element at `input0 + input1 * 8` is a hole.
    CheckElementPresent,
    /// Eager deopt if the hole bitmap of numeric storage with element base
    /// input0 marks index input1.
    CheckHoleyElementPresent(JitHoleBitmap),

    // ---- Memory ----
    /// The base address of input0's named slots: in-object or slab.
    LoadSlotBase,
    /// The named property of receiver input0 the load site at this byte PC
    /// reads through its feedback programs: one of their receivers, then one
    /// data slot of the receiver or of its guarded holder. Eager deopt when no
    /// program matches.
    LoadNamedProperty(u32),
    /// Store input1 into the named property of receiver input0 through the
    /// feedback programs of the store site at this byte PC: an existing
    /// writable slot, or an appended slot with the child shape published and
    /// its edge barriered. Eager deopt when no program matches; the value's
    /// own barrier is a separate node.
    StoreNamedProperty(u32),
    /// `[[Get]]` of the named property at the load site `pc` from receiver
    /// input0: an own or prototype data slot found in the isolate's shared
    /// lookup table for `atom`, else the full operation in the runtime.
    LoadPropertyCached {
        pc: u32,
        atom: Option<u32>,
    },
    /// `[[Set]]` of input1 as the named property of receiver input0 at the
    /// store site `pc`: an existing writable own slot found in the shared
    /// lookup table, or an add transition found in the shared transition
    /// table, for `atom`; else the full operation in the runtime.
    StorePropertyCached {
        pc: u32,
        atom: Option<u32>,
    },
    /// Tagged word at `[input0 + offset]` (input0 a `Word` base or a tagged
    /// cell address).
    LoadTaggedField(i32),
    /// Store input1 (tagged) at `[input0 + offset]`.
    StoreTaggedField(i32),
    /// Store the compressed shape handle into object input0.
    StoreShape(u32),
    /// Generational write barrier for storing input1 into object input0.
    WriteBarrier,
    /// Barrier for the tagged value input2 just stored at index input1 of the
    /// ordinary-array element base input0: a young or marking-visible cell
    /// marks the slot dirty and remembers the slab.
    ElementWriteBarrier,
    /// Parent context of context input0.
    LoadContextParent,
    /// The context closure input0 closes over, or `undefined` for a bare
    /// function value.
    LoadClosureContext,
    /// The live element count of a proved indexed receiver input0, as a
    /// `Word`.
    LoadElementsLength {
        byte: u32,
        width: JitGuardWidth,
    },
    /// The element base of a proved receiver input0, a word at this byte
    /// offset of its body.
    LoadElementsBase(u32),
    /// The element at `input0 + input1 << stride` in the element's
    /// representation: a tagged value (eager deopt on a hole), an int32, or a
    /// double. A `Uint32` element deopts eagerly above `i32::MAX`.
    LoadElement(JitElementRepr),
    /// A `Uint32` element as a double.
    LoadElementUint32ToFloat64,
    /// A double element of holey numeric storage, tagged: a hole reads
    /// `undefined` while the array-index protector holds and deopts
    /// otherwise.
    LoadHoleyFloat64Element(JitHoleBitmap),
    /// Store input2 at `input0 + input1 << stride`: tagged into a boxed
    /// element, an int32's low bits into an integer element (clamped for
    /// `Uint8Clamped`, which also takes a double), a double into a floating
    /// element.
    StoreElement(JitElementRepr),

    // ---- Calls ----
    /// `[[Call]]` of input0 with `undefined` as receiver and the remaining
    /// inputs as arguments, through the JavaScript call ABI, or with
    /// `construct` its `[[Construct]]` with input0 as `new.target`. With a
    /// `plan`, a callee proved to be the plan's function is entered through
    /// its current generation; any other callee through the generic entry.
    /// The bytecode instruction at `pc` is published while the callee runs.
    CallJs {
        pc: u32,
        plan: Option<otter_vm::jit::JitDirectCallPlan>,
        construct: bool,
        /// Input1 is the receiver; otherwise the receiver is `undefined`.
        receiver: bool,
        /// For a construct of a proven base constructor: how the receiver
        /// is allocated before the constructor is entered, so its prologue
        /// does not prepare one in the runtime.
        allocation: Option<otter_vm::jit::JitReceiverAllocationPlan>,
    },
    /// Eager deopt unless the callee of the call site at this byte PC is
    /// `%Function.prototype.call%`: input0 itself for an explicit-receiver
    /// call, or the `call` a method call reads from its closure receiver
    /// input0 through the pinned `%Function.prototype%`.
    CheckFunctionPrototypeCall(u32),
    /// Eager deopt unless input0 is the function `function_id`: its
    /// function-id immediate, or a closure of it that needs no runtime setup.
    /// `cell`, when not zero, holds the last value proved here, which a
    /// repeated callee matches with one compare.
    CheckFunction {
        function_id: u32,
        cell: u64,
    },
    /// Eager deopt when input0 is the hole of a binding still in its
    /// temporal dead zone.
    CheckNotHole,
    /// `input0 instanceof input1`: the prototype chain walk of the default
    /// `@@hasInstance` inline, every other case in the runtime.
    Instanceof,
    /// The method the guarded method call at this byte PC reads from
    /// receiver input0: its proved shape, prototype chain and holder slot.
    /// Eager deopt when the receiver does not match.
    LoadGuardedMethod {
        byte_pc: u32,
        /// The site's method target whose guard and slot this load uses.
        target: u8,
        /// The receiver is already proved an ordinary object of the
        /// target's shape.
        receiver_proved: bool,
    },
    /// The compressed shape of input0 when it is an ordinary object that
    /// looks its properties up through its shape; zero otherwise.
    LoadReceiverShape,
    /// The baseline operation for the bytecode instruction at `pc`, run on
    /// the frame window: input `i` is stored to window register
    /// `registers[i]` first, and the instruction's written registers are read
    /// back by `LoadWindow` nodes after it.
    Generic {
        pc: u32,
        registers: Box<[u16]>,
    },

    // ---- Control ----
    Jump(BlockId),
    /// Back edge to a loop header; carries the interrupt poll.
    JumpLoop(BlockId),
    Branch {
        kind: BranchKind,
        if_true: BlockId,
        if_false: BlockId,
    },
    /// Return input0 to the caller.
    Return,
    /// Unconditional eager deopt.
    Deopt(DeoptReason),
}

/// What a node may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Properties {
    /// Clobbers every allocatable register (a full call).
    pub(crate) call: bool,
    pub(crate) eager_deopt: bool,
    pub(crate) lazy_deopt: bool,
    pub(crate) can_throw: bool,
    /// Writes heap state another node may read.
    pub(crate) writes: bool,
    /// Must stay even when its value is unused.
    pub(crate) effectful: bool,
    /// Not a call, but its slow path enters the runtime, which may collect
    /// or run JavaScript: live registers are saved in rooted snapshot slots
    /// around it and its safepoint roots every live tagged slot.
    pub(crate) may_collect: bool,
}

/// Where the allocator must put one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputPolicy {
    /// Any general or floating-point register matching the input's repr.
    Register,
    /// Register, spill slot, or constant.
    Any,
    /// This general register.
    FixedGp(u8),
}

/// Where the allocator must put the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultPolicy {
    None,
    Register,
    FixedGp(u8),
    /// The same register as input `index`, which must die here.
    SameAsInput(u8),
}

/// Register-allocation contract of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Constraints {
    pub(crate) inputs: SmallVec<[InputPolicy; 4]>,
    pub(crate) result: ResultPolicy,
    pub(crate) gp_temps: u8,
    pub(crate) fp_temps: u8,
}

impl Kind {
    /// Whether the node's tagged result is always `true` or `false`.
    pub(crate) fn produces_boolean(&self) -> bool {
        matches!(
            self,
            Self::Int32Compare(_)
                | Self::Float64Compare(_)
                | Self::TaggedEqual
                | Self::StrictEqual { .. }
                | Self::ToBoolean
                | Self::LogicalNot
        )
    }

    pub(crate) fn is_constant(&self) -> bool {
        matches!(
            self,
            Self::ConstTagged(_) | Self::ConstInt32(_) | Self::ConstFloat64(_)
        )
    }

    pub(crate) fn is_control(&self) -> bool {
        matches!(
            self,
            Self::Jump(_) | Self::JumpLoop(_) | Self::Branch { .. } | Self::Return | Self::Deopt(_)
        )
    }

    pub(crate) fn properties(&self) -> Properties {
        let eager = Properties {
            eager_deopt: true,
            ..Properties::default()
        };
        let pure = Properties::default();
        match self {
            Self::ConstTagged(_)
            | Self::ConstInt32(_)
            | Self::ConstFloat64(_)
            | Self::InitialRegister(_)
            | Self::LoadThis
            | Self::LoadClosure
            | Self::LoadStringConstant(_)
            | Self::LoadNewTarget
            | Self::LoadWindow(_)
            | Self::Phi
            | Self::Int32BitAnd
            | Self::Int32BitOr
            | Self::Int32BitXor
            | Self::Int32BitNot
            | Self::Int32ShiftLeft
            | Self::Int32ShiftRight
            | Self::Uint32ShiftRightToFloat64
            | Self::Int32Compare(_)
            | Self::Float64Add
            | Self::Float64Sub
            | Self::Float64Mul
            | Self::Float64Div
            | Self::Float64Negate
            | Self::Float64Compare(_)
            | Self::Int32ToTagged
            | Self::Float64ToTagged
            | Self::Int32ToFloat64
            | Self::TruncateFloat64ToInt32
            | Self::TaggedEqual
            | Self::StrictEqual { .. }
            | Self::ToBoolean
            | Self::LogicalNot
            | Self::LoadSlotBase
            | Self::LoadTaggedField(_)
            | Self::LoadContextParent
            | Self::LoadClosureContext
            | Self::LoadElementsLength { .. }
            | Self::LoadElementsBase(_)
            | Self::LoadReceiverShape
            | Self::LoadElementUint32ToFloat64 => pure,
            Self::LoadElement(element) => {
                if matches!(element, JitElementRepr::Boxed | JitElementRepr::Uint32) {
                    eager
                } else {
                    pure
                }
            }
            Self::Float64Mod => pure,
            Self::Int32Add
            | Self::Int32Sub
            | Self::Int32Mul
            | Self::Int32Div
            | Self::Int32Mod
            | Self::Int32Negate
            | Self::Int32ShiftRightLogical
            | Self::CheckedTaggedToInt32
            | Self::CheckedTaggedToFloat64
            | Self::CheckedFloat64ToInt32
            | Self::CheckedTaggedToIndex
            | Self::CheckedFloat64ToIndex
            | Self::CheckHeapObject
            | Self::CheckNumber
            | Self::CheckShapes { .. }
            | Self::CheckValue(_)
            | Self::CheckBounds
            | Self::CheckFunctionPrototypeCall(_)
            | Self::CheckFunction { .. }
            | Self::CheckNotHole
            | Self::LoadGuardedMethod { .. }
            | Self::LoadNamedProperty(_)
            | Self::CheckElements { .. }
            | Self::CheckElementPresent
            | Self::CheckHoleyElementPresent(_)
            | Self::LoadHoleyFloat64Element(_) => eager,
            Self::StoreNamedProperty(_) => Properties {
                eager_deopt: true,
                writes: true,
                effectful: true,
                ..Properties::default()
            },
            Self::LoadPropertyCached { .. }
            | Self::StorePropertyCached { .. }
            | Self::Instanceof => Properties {
                eager_deopt: true,
                can_throw: true,
                writes: true,
                effectful: true,
                may_collect: true,
                ..Properties::default()
            },
            Self::StoreWindow(_)
            | Self::StoreTaggedField(_)
            | Self::StoreShape(_)
            | Self::StoreElement(_)
            | Self::WriteBarrier
            | Self::ElementWriteBarrier => Properties {
                writes: true,
                effectful: true,
                ..Properties::default()
            },
            Self::CallJs { .. } | Self::Generic { .. } => Properties {
                call: true,
                eager_deopt: true,
                lazy_deopt: true,
                can_throw: true,
                writes: true,
                effectful: true,
                may_collect: false,
            },
            Self::Jump(_) | Self::JumpLoop(_) | Self::Branch { .. } | Self::Return => Properties {
                effectful: true,
                ..Properties::default()
            },
            Self::Deopt(_) => Properties {
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
        }
    }

    /// The allocation contract for a node with `input_count` inputs.
    pub(crate) fn constraints(&self, input_count: usize) -> Constraints {
        use InputPolicy::{Any, FixedGp, Register};
        let registers =
            |count: usize| -> SmallVec<[InputPolicy; 4]> { (0..count).map(|_| Register).collect() };
        let simple = |inputs: usize, result: ResultPolicy| Constraints {
            inputs: registers(inputs),
            result,
            gp_temps: 0,
            fp_temps: 0,
        };
        match self {
            Self::ConstTagged(_) | Self::ConstInt32(_) | Self::ConstFloat64(_) | Self::Phi => {
                Constraints {
                    inputs: (0..input_count).map(|_| Any).collect(),
                    result: ResultPolicy::Register,
                    gp_temps: 0,
                    fp_temps: 0,
                }
            }
            Self::InitialRegister(_)
            | Self::LoadThis
            | Self::LoadClosure
            | Self::LoadStringConstant(_)
            | Self::LoadNewTarget
            | Self::LoadWindow(_) => simple(0, ResultPolicy::Register),
            Self::StoreWindow(_) => simple(1, ResultPolicy::None),
            Self::Int32Add
            | Self::Int32Sub
            | Self::Int32Mul
            | Self::Int32BitAnd
            | Self::Int32BitOr
            | Self::Int32BitXor
            | Self::Int32ShiftLeft
            | Self::Int32ShiftRight
            | Self::Int32ShiftRightLogical
            | Self::Uint32ShiftRightToFloat64
            | Self::Int32Compare(_)
            | Self::Float64Add
            | Self::Float64Sub
            | Self::Float64Mul
            | Self::Float64Div
            | Self::Float64Compare(_)
            | Self::TaggedEqual
            | Self::LoadElement(_)
            | Self::LoadElementUint32ToFloat64 => simple(2, ResultPolicy::Register),
            Self::LoadHoleyFloat64Element(_) => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 1,
            },
            Self::CheckBounds | Self::CheckElementPresent | Self::CheckHoleyElementPresent(_) => {
                simple(2, ResultPolicy::None)
            }
            Self::StoreElement(_) | Self::ElementWriteBarrier => simple(3, ResultPolicy::None),
            Self::CheckFunctionPrototypeCall(_) => Constraints {
                inputs: registers(1),
                result: ResultPolicy::None,
                gp_temps: 1,
                fp_temps: 0,
            },
            Self::CheckFunction { .. } | Self::CheckNotHole => simple(1, ResultPolicy::None),
            Self::LoadGuardedMethod { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 1,
                fp_temps: 0,
            },
            Self::LoadNamedProperty(_) => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 2,
                fp_temps: 0,
            },
            Self::StoreNamedProperty(_) => Constraints {
                inputs: registers(2),
                result: ResultPolicy::None,
                gp_temps: 2,
                fp_temps: 0,
            },
            Self::Instanceof => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 5,
                fp_temps: 0,
            },
            Self::LoadPropertyCached { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 4,
                fp_temps: 0,
            },
            Self::StorePropertyCached { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::None,
                gp_temps: 4,
                fp_temps: 0,
            },
            Self::Int32Div | Self::Int32Mod => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 1,
                fp_temps: 0,
            },
            Self::StrictEqual { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 1,
            },
            Self::Float64Mod => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
            },
            Self::Int32Negate
            | Self::Int32BitNot
            | Self::Float64Negate
            | Self::CheckedTaggedToInt32
            | Self::Int32ToTagged
            | Self::Int32ToFloat64
            | Self::TruncateFloat64ToInt32
            | Self::CheckedFloat64ToInt32
            | Self::CheckedFloat64ToIndex
            | Self::LoadSlotBase
            | Self::LoadTaggedField(_)
            | Self::LoadContextParent
            | Self::LoadClosureContext
            | Self::LoadElementsLength { .. }
            | Self::LoadElementsBase(_)
            | Self::LoadReceiverShape => simple(1, ResultPolicy::Register),
            Self::CheckedTaggedToFloat64 | Self::Float64ToTagged | Self::CheckedTaggedToIndex => {
                Constraints {
                    inputs: registers(1),
                    result: ResultPolicy::Register,
                    gp_temps: 0,
                    fp_temps: 1,
                }
            }
            Self::ToBoolean | Self::LogicalNot => simple(1, ResultPolicy::Register),
            Self::CheckHeapObject | Self::CheckNumber | Self::CheckValue(_) => {
                simple(1, ResultPolicy::None)
            }
            Self::CheckShapes { .. } | Self::CheckElements { .. } => simple(1, ResultPolicy::None),
            Self::StoreTaggedField(_) => simple(2, ResultPolicy::None),
            Self::StoreShape(_) => simple(1, ResultPolicy::None),
            Self::WriteBarrier => simple(2, ResultPolicy::None),
            Self::CallJs { receiver, .. } => {
                let mut inputs: SmallVec<[InputPolicy; 4]> = smallvec::smallvec![FixedGp(1)];
                if *receiver {
                    inputs.push(FixedGp(2));
                }
                inputs.extend((inputs.len()..input_count).map(|_| Any));
                Constraints {
                    inputs,
                    result: ResultPolicy::FixedGp(0),
                    gp_temps: 0,
                    fp_temps: 0,
                }
            }
            Self::Generic { .. } => Constraints {
                inputs: (0..input_count).map(|_| Any).collect(),
                result: ResultPolicy::FixedGp(0),
                gp_temps: 0,
                fp_temps: 0,
            },
            Self::Jump(_) | Self::JumpLoop(_) | Self::Deopt(_) => simple(0, ResultPolicy::None),
            Self::Branch { .. } => simple(input_count, ResultPolicy::None),
            Self::Return => Constraints {
                inputs: smallvec::smallvec![FixedGp(0)],
                result: ResultPolicy::None,
                gp_temps: 0,
                fp_temps: 0,
            },
        }
    }
}

/// One node in the arena.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub(crate) kind: Kind,
    pub(crate) inputs: SmallVec<[NodeId; 3]>,
    pub(crate) repr: Repr,
    /// Frame state resumed before this node's instruction.
    pub(crate) eager: Option<FrameStateId>,
    /// Frame state resumed after this node's instruction.
    pub(crate) lazy: Option<FrameStateId>,
    /// Owning block; `None` for constants.
    pub(crate) block: Option<BlockId>,
    /// The bytecode instruction this node was built for, in its own body.
    pub(crate) pc: u32,
    /// The function body the node belongs to: `0` for the compiled
    /// function, `i` for [`Graph::inlined`]`[i - 1]`.
    pub(crate) origin: u16,
}

/// One basic block.
#[derive(Debug, Clone, Default)]
pub(crate) struct Block {
    pub(crate) predecessors: Vec<BlockId>,
    pub(crate) phis: Vec<NodeId>,
    pub(crate) body: Vec<NodeId>,
    /// The terminating control node.
    pub(crate) control: Option<NodeId>,
    /// Loop header whose last predecessor is the back edge.
    pub(crate) is_loop: bool,
}

/// Interpreter state to rebuild at a deopt.
#[derive(Debug, Clone)]
pub(crate) struct FrameState {
    /// Logical PC the interpreter resumes at.
    pub(crate) pc: u32,
    /// Byte PC of `pc`, for the VM's frame records.
    pub(crate) byte_pc: u32,
    pub(crate) function_id: u32,
    /// Interpreter register count of the frame.
    pub(crate) register_count: u16,
    /// Live registers and their values; every other register resumes as
    /// `undefined`.
    pub(crate) registers: Vec<(u16, NodeId)>,
    /// How the frame was entered when it runs an inlined call; `None` for
    /// the compiled function's own frame.
    pub(crate) caller: Option<InlineCaller>,
}

impl FrameState {
    /// Apply `f` to every value this frame itself reads: its entry bindings,
    /// then its live registers.
    pub(crate) fn for_each_value_mut(&mut self, mut f: impl FnMut(&mut NodeId)) {
        if let Some(caller) = &mut self.caller {
            f(&mut caller.this);
            f(&mut caller.closure);
            f(&mut caller.new_target);
        }
        for (_, value) in &mut self.registers {
            f(value);
        }
    }
}

/// The call an inlined frame runs for.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InlineCaller {
    /// The calling frame's state. It resumes after the call and stands on
    /// the call until this frame returns.
    pub(crate) state: FrameStateId,
    /// The calling frame's register the call writes.
    pub(crate) return_register: u16,
    /// The frame's `this` binding.
    pub(crate) this: NodeId,
    /// The exact callable the frame runs.
    pub(crate) closure: NodeId,
    /// The frame's `new.target`.
    pub(crate) new_target: NodeId,
}

/// A point graph construction can return to; see [`Graph::checkpoint`].
#[derive(Debug)]
pub(crate) struct Checkpoint {
    nodes: usize,
    blocks: usize,
    frame_states: usize,
    inlined: usize,
    constants: rustc_hash::FxHashMap<(u8, u64), NodeId>,
    pub(crate) block: BlockId,
    body: usize,
}

/// One function body inlined into the graph.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InlinedBody {
    pub(crate) function_id: u32,
    /// The origin of the body that calls it.
    pub(crate) parent: u16,
    /// The call instruction in the calling body: logical and byte PC.
    pub(crate) call_pc: u32,
    pub(crate) call_byte_pc: u32,
}

/// The whole graph of one compilation.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    pub(crate) nodes: Vec<Node>,
    pub(crate) blocks: Vec<Block>,
    pub(crate) frame_states: Vec<FrameState>,
    constants: rustc_hash::FxHashMap<(u8, u64), NodeId>,
    /// The instruction new nodes are built for.
    pub(crate) position: u32,
    /// The body new nodes are built for.
    pub(crate) origin: u16,
    /// Inlined bodies; origin `i > 0` names `inlined[i - 1]`.
    pub(crate) inlined: Vec<InlinedBody>,
}

impl Graph {
    pub(crate) fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.0 as usize]
    }

    pub(crate) fn node_mut(&mut self, id: NodeId) -> &mut Node {
        &mut self.nodes[id.0 as usize]
    }

    pub(crate) fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }

    pub(crate) fn block_mut(&mut self, id: BlockId) -> &mut Block {
        &mut self.blocks[id.0 as usize]
    }

    pub(crate) fn frame_state(&self, id: FrameStateId) -> &FrameState {
        &self.frame_states[id.0 as usize]
    }

    pub(crate) fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block::default());
        id
    }

    pub(crate) fn add_frame_state(&mut self, state: FrameState) -> FrameStateId {
        let id = FrameStateId(self.frame_states.len() as u32);
        self.frame_states.push(state);
        id
    }

    /// Append a node without placing it.
    pub(crate) fn add_node(&mut self, kind: Kind, inputs: &[NodeId], repr: Repr) -> NodeId {
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            kind,
            inputs: inputs.iter().copied().collect(),
            repr,
            eager: None,
            lazy: None,
            block: None,
            pc: self.position,
            origin: self.origin,
        });
        id
    }

    /// The frame states of `id`'s inline chain, outermost first.
    pub(crate) fn state_chain(&self, id: FrameStateId) -> SmallVec<[FrameStateId; 4]> {
        let mut chain = SmallVec::new();
        let mut cursor = Some(id);
        while let Some(state) = cursor {
            chain.push(state);
            cursor = self.frame_state(state).caller.map(|caller| caller.state);
        }
        chain.reverse();
        chain
    }

    /// Every value the frame states of `id`'s chain read, outermost frame
    /// first: an inlined frame's entry bindings (`this`, closure,
    /// `new.target`), then each frame's live registers.
    pub(crate) fn state_values(&self, id: FrameStateId) -> SmallVec<[NodeId; 16]> {
        let mut values = SmallVec::new();
        for state in self.state_chain(id) {
            let data = self.frame_state(state);
            if let Some(caller) = data.caller {
                values.extend([caller.this, caller.closure, caller.new_target]);
            }
            values.extend(data.registers.iter().map(|&(_, value)| value));
        }
        values
    }

    /// The instruction of the compiled function itself that `node` runs
    /// under: its own for a node of that function, the outermost inlined
    /// call for a node of an inlined body.
    pub(crate) fn outer_pc(&self, node: NodeId) -> u32 {
        let data = self.node(node);
        self.outer_pc_at(data.origin, data.pc)
    }

    /// [`Self::outer_pc`] of instruction `pc` of the body `origin`.
    pub(crate) fn outer_pc_at(&self, mut origin: u16, mut pc: u32) -> u32 {
        while origin != 0 {
            let body = self.inlined[usize::from(origin) - 1];
            (origin, pc) = (body.parent, body.call_pc);
        }
        pc
    }

    /// The state [`Self::rollback`] returns the graph to, with `block`
    /// the open block construction continues in.
    pub(crate) fn checkpoint(&self, block: BlockId) -> Checkpoint {
        let data = self.block(block);
        Checkpoint {
            nodes: self.nodes.len(),
            blocks: self.blocks.len(),
            frame_states: self.frame_states.len(),
            inlined: self.inlined.len(),
            constants: self.constants.clone(),
            block,
            body: data.body.len(),
        }
    }

    /// Drop everything built since `checkpoint` and reopen its block.
    pub(crate) fn rollback(&mut self, checkpoint: Checkpoint) {
        self.nodes.truncate(checkpoint.nodes);
        self.blocks.truncate(checkpoint.blocks);
        self.frame_states.truncate(checkpoint.frame_states);
        self.inlined.truncate(checkpoint.inlined);
        self.constants = checkpoint.constants;
        let block = self.block_mut(checkpoint.block);
        block.body.truncate(checkpoint.body);
        block.control = None;
    }

    /// The canonical constant node for `kind`.
    pub(crate) fn constant(&mut self, kind: Kind) -> NodeId {
        let (key, repr) = match kind {
            Kind::ConstTagged(bits) => ((0u8, bits), Repr::Tagged),
            Kind::ConstInt32(value) => ((1u8, u64::from(value as u32)), Repr::Int32),
            Kind::ConstFloat64(bits) => ((2u8, bits), Repr::Float64),
            _ => unreachable!("not a constant kind"),
        };
        if let Some(&id) = self.constants.get(&key) {
            return id;
        }
        let id = self.add_node(kind, &[], repr);
        self.constants.insert(key, id);
        id
    }

    /// Constant value of `id`, if it is a constant node.
    pub(crate) fn constant_int32(&self, id: NodeId) -> Option<i32> {
        match self.node(id).kind {
            Kind::ConstInt32(value) => Some(value),
            _ => None,
        }
    }
}

impl Graph {
    /// A readable listing of the blocks in `layout`, for tests and
    /// diagnostics.
    pub(crate) fn dump(&self, layout: &[BlockId]) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for &block in layout {
            let data = self.block(block);
            let _ = writeln!(
                out,
                "b{}{} preds={:?}",
                block.0,
                if data.is_loop { " loop" } else { "" },
                data.predecessors.iter().map(|b| b.0).collect::<Vec<_>>()
            );
            for &node in data
                .phis
                .iter()
                .chain(&data.body)
                .chain(data.control.iter())
            {
                let n = self.node(node);
                let _ = writeln!(
                    out,
                    "  v{} = {:?} {:?} {:?}{}",
                    node.0,
                    n.kind,
                    n.inputs.iter().map(|i| i.0).collect::<Vec<_>>(),
                    n.repr,
                    n.eager.map_or(String::new(), |s| format!(
                        " eager={:?}",
                        self.state_chain(s)
                            .iter()
                            .map(|&state| self
                                .frame_state(state)
                                .registers
                                .iter()
                                .map(|(r, v)| (*r, v.0))
                                .collect::<Vec<_>>())
                            .collect::<Vec<_>>()
                    ))
                );
            }
        }
        out
    }
}
