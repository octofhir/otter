//! Graph IR of the graph tier: SSA value nodes in basic blocks, each block
//! ending in one control node, with deopt frame states attached to the nodes
//! that can leave optimized code.
//!
//! # Contents
//! - [`NodeId`], [`BlockId`], [`FrameStateId`] — dense arena indices.
//! - [`Repr`] — the machine representation of a node's result.
//! - [`Kind`] — every node operation, value and control alike.
//! - [`Node`], [`Block`], [`FrameState`], [`Graph`] — the arena.
//! - [`super::dump`] — opt-in complete constant and block declarations.
//! - [`InlineCaller`] / [`InlinedBody`] — how an inlined body's frames and
//!   nodes name the call they run for.
//! - [`Properties`] / [`Kind::properties`] — what a node may do (call,
//!   deopt eagerly or lazily, throw, read or write the heap).
//! - [`Constraints`] / [`Kind::constraints`] — where the register allocator
//!   must put inputs, the result, and how many temporaries a node needs.
//! - Named-load plans retain ordered indices into their source body's programs.
//! - Field-summary checks authorize nullable value bounds; own stores select
//!   their defining field cell by the receiver's admitted shape.
//!
//! # Invariants
//! - Constants live outside blocks and are materialized at each use; every
//!   other retained node is placed in exactly one block. Removed checks have
//!   no block or eager state and are absent from every block's body.
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
//! - A named load's receiver shape is a live exact proof. Only a matching
//!   receiver shape guard may be omitted; holder and chain proofs stay live.
//! - A field-summary check reads metadata, never a field's current value.
//!   Its exact ordinary targets still need shape stability confirmation.
//!
//! # See also
//! - [`super::builder`] — builds the graph from bytecode and feedback.
//! - [`super::regalloc`] — assigns locations from [`Constraints`].

use otter_vm::jit::{
    JitBodyGuard, JitElementRepr, JitFieldTypeGuard, JitGuardWidth, JitHoleBitmap,
};
use smallvec::SmallVec;

/// Dense node index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct NodeId(pub(crate) u32);

pub(crate) use crate::field_store::FieldStoreOwner;

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
    /// A raw machine word that is never a GC root: a length, bit pattern, or
    /// transient storage address. A movable storage address must be consumed
    /// before a collecting call and reloaded from its rooted owner afterward.
    Word,
}

impl Repr {
    pub(crate) fn is_float(self) -> bool {
        self == Self::Float64
    }
}

/// A condition on numeric operands or the VM primitive ordering result.
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
    /// A shape fact the generation subscribed to changed: its dependency
    /// flag is set.
    DependencyChanged,
    /// A tracked literal allocation site has decided its placement.
    AllocationSiteDecided,
}

/// One branch condition of a [`Kind::Branch`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum BranchKind {
    /// `ToBoolean(input0)`.
    Truthy,
    /// `input0 cond input1` on int32 operands.
    Int32(Condition),
    /// `input0 cond input1` on float64 operands; unordered satisfies only `!=`.
    Float64(Condition),
    /// `ToBoolean` of the float64 `input0`: neither zero nor NaN.
    Float64Truthy,
    /// `input0` and `input1` are the same tagged word.
    TaggedEqual,
    /// `input0` is `undefined` or `null`.
    Nullish,
    /// The word `input0` equals this constant.
    WordEqual(u32),
    /// `input0` is the native function whose external reference index is
    /// this constant.
    Native(u32),
}

/// A `Math` method on float64 operands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MathOp {
    Abs,
    Sqrt,
    Floor,
    Ceil,
    Trunc,
    Round,
    Min,
    Max,
    Sin,
    Cos,
    Tan,
    Atan,
    Exp,
    Log,
    Atan2,
    Pow,
}

impl MathOp {
    /// The method a static native `Math` leaf computes on Number operands.
    pub(crate) fn of_leaf(stub: otter_vm::native_abi::RuntimeStubId) -> Option<Self> {
        use otter_vm::native_abi as abi;
        Some(match stub {
            id if id == abi::STUB_MATH_ABS_LEAF.id => Self::Abs,
            id if id == abi::STUB_MATH_SQRT_LEAF.id => Self::Sqrt,
            id if id == abi::STUB_MATH_FLOOR_LEAF.id => Self::Floor,
            id if id == abi::STUB_MATH_CEIL_LEAF.id => Self::Ceil,
            id if id == abi::STUB_MATH_TRUNC_LEAF.id => Self::Trunc,
            id if id == abi::STUB_MATH_ROUND_LEAF.id => Self::Round,
            id if id == abi::STUB_MATH_MIN_LEAF.id => Self::Min,
            id if id == abi::STUB_MATH_MAX_LEAF.id => Self::Max,
            id if id == abi::STUB_MATH_SIN_LEAF.id => Self::Sin,
            id if id == abi::STUB_MATH_COS_LEAF.id => Self::Cos,
            id if id == abi::STUB_MATH_TAN_LEAF.id => Self::Tan,
            id if id == abi::STUB_MATH_ATAN_LEAF.id => Self::Atan,
            id if id == abi::STUB_MATH_EXP_LEAF.id => Self::Exp,
            id if id == abi::STUB_MATH_LOG_LEAF.id => Self::Log,
            id if id == abi::STUB_MATH_ATAN2_LEAF.id => Self::Atan2,
            id if id == abi::STUB_MATH_POW_LEAF.id => Self::Pow,
            _ => return None,
        })
    }

    /// The operands the method reads.
    pub(crate) fn arity(self) -> usize {
        match self {
            Self::Min | Self::Max | Self::Atan2 | Self::Pow => 2,
            _ => 1,
        }
    }

    /// The method's typed id, as the shared `Math` leaf takes it.
    pub(crate) fn method(self) -> otter_bytecode::method_id::MathMethod {
        use otter_bytecode::method_id::MathMethod as M;
        match self {
            Self::Abs => M::Abs,
            Self::Sqrt => M::Sqrt,
            Self::Floor => M::Floor,
            Self::Ceil => M::Ceil,
            Self::Trunc => M::Trunc,
            Self::Round => M::Round,
            Self::Min => M::Min,
            Self::Max => M::Max,
            Self::Sin => M::Sin,
            Self::Cos => M::Cos,
            Self::Tan => M::Tan,
            Self::Atan => M::Atan,
            Self::Exp => M::Exp,
            Self::Log => M::Log,
            Self::Atan2 => M::Atan2,
            Self::Pow => M::Pow,
        }
    }
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
    /// This activation's explicit new.target binding.
    LoadNewTarget,
    /// The callee (SELF) from the record.
    LoadClosure,
    /// The string or BigInt literal loaded at this byte PC, read from its
    /// isolate-owned cell.
    LoadLiteral(u32),
    /// Live global binding at this source body's byte PC. A pre-effect
    /// guard resumes the canonical binding operation at that exact site.
    LoadGlobalBinding(u32),
    /// The global object of this source body's realm; eager deopt while
    /// another realm is active.
    LoadGlobalThis,
    /// Read a window-resident register.
    LoadWindow(u16),
    /// A merge of one value per predecessor.
    Phi,

    // ---- Allocation ----
    /// One post-LICM fixed-shell group, preserving the first original source id.
    AllocationGroup(u32),
    /// Tagged cell at this byte offset from a fully published group first cell.
    AllocationProjection(u32),
    /// Fresh ordinary empty object in this exact source body's realm.
    NewObject,
    /// Fresh empty array shell; extra realms use the committed sidecar allocator.
    NewArrayEmpty,
    /// Static own-property values initialized from canonical tagged homes.
    /// `fields` holds one selected record per input in source order when the
    /// source body's geometry admits the native fit; otherwise only the
    /// committed allocation runs.
    NewObjectLiteral {
        fields: Option<SmallVec<[FieldStoreOwner; 4]>>,
    },
    /// Dense source-order elements initialized from canonical tagged homes.
    NewArrayLiteral,
    /// Fresh lexical environment for this source function's scope.
    NativeNewContext(u32),
    /// Per-iteration environment copy from a rooted context operand.
    CopyContext,
    /// Fresh callable; exact source owns the template, inputs bind context/this/new.target.
    NewClosure,
    /// The receiver of an inlined `[[Construct]]` whose new.target is
    /// input0: the plan's shape and in-object slots, all `undefined`, once
    /// the live family and prototype are proved. A failed proof, or a
    /// buffer the collector cannot refill, resumes the construct.
    NewReceiver(otter_vm::jit::JitReceiverAllocationPlan),
    /// The proofs of a [`Self::NewReceiver`] whose allocation escape analysis
    /// removed: the live `new.target` (input0) is the plan's family and its
    /// prototype holds. Eager deopt otherwise, which resumes the construct.
    CheckReceiverPlan(otter_vm::jit::JitReceiverAllocationPlan),
    /// A scalar-replaced ordinary object of the exact hidden class `shape`,
    /// whose slots hold the inputs in order. It is never placed or emitted:
    /// frame states name it, and a deopt materializes it.
    VirtualObject {
        shape: u32,
    },
    /// `ArrayCreate(input0)` for the original `%Array%` constructor called
    /// with at most one argument: an array of that exact nonnegative int32
    /// length, all holes. Any other length resumes the operation.
    NewArrayWithLength,

    /// Number/String primitive addition with a collecting string miss.
    PrimitiveAdd,
    /// A binary operator over two BigInts; any other operand exits.
    BigIntBinary(otter_vm::bigint::ops::Operator),
    /// Primitive Number/String order, including unordered numeric operands.
    PrimitiveCompare(Condition),

    // ---- Int32 arithmetic ----
    /// Checked add; eager deopt on overflow.
    Int32Add,
    Int32Sub,
    /// Unchecked two's-complement add: the exact sum where the operands'
    /// ranges prove it fits int32, otherwise the `ToInt32` of it that every
    /// use applies (and no frame state records).
    Int32AddWrapping,
    /// Unchecked two's-complement subtract under the same contract.
    Int32SubWrapping,
    /// Checked multiply; eager deopt on overflow and on a `-0` product.
    Int32Mul,
    /// Unchecked multiply whose operands' ranges prove the product fits
    /// int32 and is not `-0`, or whose uses all identify `-0` with `0`.
    Int32MulExact,
    /// Multiply that deopts on overflow only: every use identifies a `-0`
    /// product with `0`.
    Int32MulIdentifyZeros,
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
    /// `Math.<op>` of [`MathOp::arity`] float64 inputs, producing a float64:
    /// one instruction where the target has it, otherwise the shared typed
    /// `Math` leaf with every live register saved around it.
    Float64Math(MathOp),
    /// Float64 compare producing a tagged boolean; unordered satisfies only `!=`.
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
    /// `input0 === input1` (or `!==` when `negate`) on any two tagged values,
    /// producing a tagged boolean: numbers by value, strings and BigInts by
    /// content through a leaf probe, everything else by identity.
    StrictEqual {
        negate: bool,
    },
    /// `input0 == input1` (or `!=` when `negate`) on any two tagged values,
    /// producing a tagged boolean: identity, two Numbers, `null`/`undefined`
    /// and two cells that convert nothing inline, every coercing pair in
    /// the runtime, which may run JavaScript.
    LooseEqual {
        negate: bool,
    },
    /// Loose equality against null or undefined, including native HTML-DDA
    /// cells, without conversion, allocation or JavaScript execution.
    NullishEqual {
        negate: bool,
        /// Ordinary cells and nullish values exclude native HTML-DDA identity.
        object_or_nullish: bool,
    },
    /// `ToBoolean` producing a tagged boolean.
    ToBoolean,
    /// `!ToBoolean` producing a tagged boolean.
    LogicalNot,
    /// `typeof input0 === kind` (or `!==`), the encoded
    /// [`otter_bytecode::TypeOfTest`], producing a tagged boolean: decided
    /// from the value bits and the cell's type tag, with a leaf probe for
    /// the cells only the heap decides.
    TestTypeOf {
        test: i32,
    },

    // ---- Checks ----
    /// Eager deopt unless input0 is a Number.
    CheckNumber,
    /// Eager deopt if input0 is one of the nullish categories in `mask`.
    CheckNotNullish {
        mask: u8,
    },
    /// The builder's proof that every shape-owned field summary still
    /// matches its snapshot. The generation subscribes to these summaries;
    /// [`super::dependency_checks`] turns the node into a
    /// [`Kind::CheckDependencies`] or removes it before allocation.
    CheckFieldTypes(SmallVec<[JitFieldTypeGuard; 4]>),
    /// Eager deopt unless input0 is an ordinary object with one of the
    /// shape handles (compressed offsets) and no object-local state
    /// overriding its slots; `writable` also requires that the object is
    /// not a prototype (whose slot writes must invalidate dependents), which
    /// its shapes already prove: they come from store handlers, which no
    /// prototype shape receives, and a shape's state never changes.
    /// The compare reads input0's map word (`otter_gc::cell_map`), which no
    /// cell other than an object of one of these shapes holds; `cell_proved`
    /// marks input0 as already proved a heap cell on every path here,
    /// leaving one load and the compares.
    CheckShapes {
        shapes: SmallVec<[u32; 4]>,
        writable: bool,
        cell_proved: bool,
    },
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
    /// The boxed element at `input0 + input1 * 8`; eager deopt if it is a
    /// hole, which only the committed prototype lookup may answer.
    LoadPresentElement,
    /// Eager deopt if the hole bitmap of numeric storage with element base
    /// input0 marks index input1.
    CheckHoleyElementPresent(JitHoleBitmap),

    // ---- Memory ----
    /// Read input0's shape-owned field without exposing a movable storage
    /// address. `atom` names the property, which is the field's alias class.
    LoadOwnField {
        field: otter_vm::object::FieldLocation,
        atom: u32,
    },
    /// Tagged live named storage of actual Array input0; miss before effects.
    LoadArrayExpando(otter_vm::jit::JitArrayLayout),
    /// The UTF-16 code unit of string input0 at int32 index input1, as an
    /// int32: the string is flat (inline or sequential, Latin-1 or UTF-16)
    /// or a slice of a flat string, and the index lies within it. Eager
    /// deopt otherwise.
    StringCharCodeAt,
    /// The own-property bag of the closure or native function input0 whose
    /// named lookup starts there: the bag exists, no `[[Prototype]]` override
    /// is installed and the default realm is active. Eager deopt otherwise.
    LoadFunctionBag,
    /// Store input1 into input0's shape-owned field; a separate node owns its barrier.
    /// `owners` pairs each admitted receiver shape with its selected record;
    /// it is empty when no arm observes.
    StoreOwnField {
        field: otter_vm::object::FieldLocation,
        atom: u32,
        owners: SmallVec<[(u32, FieldStoreOwner); 4]>,
    },
    /// Read a named property of input0 through the selected feedback programs
    /// at `byte_pc`. Indices refer to the original source programs in their
    /// order. A live exact `receiver_shape` proves only a matching receiver
    /// shape guard; holder and chain guards remain. Eager deopt when no
    /// selected program matches.
    LoadNamedProperty {
        byte_pc: u32,
        programs: SmallVec<[u32; 4]>,
        receiver_shape: Option<u32>,
    },
    /// Store input1 into the named property of receiver input0 through the
    /// feedback programs of the store site at `byte_pc`: an existing
    /// writable slot, or an appended slot with the child shape published and
    /// its edge barriered. `owners` holds one selected record per source
    /// program, in program order. A live exact `receiver_shape` proves the
    /// receiver an ordinary object of that shape: only the programs that
    /// guard it run, without that guard. Eager deopt when no program
    /// matches; the value's own barrier is a separate node. A `fresh`
    /// receiver was bump-allocated on this path with no call, collection or
    /// poll since, so no code can have given it a prototype role or made it
    /// non-extensible: neither is re-checked.
    StoreNamedProperty {
        byte_pc: u32,
        owners: SmallVec<[FieldStoreOwner; 4]>,
        receiver_shape: Option<u32>,
        fresh: bool,
    },
    /// Store input1 into the own data property receiver input0 keeps in an
    /// exotic body slot (a RegExp's `lastIndex`). Eager deopt unless input0
    /// is a cell of the slot's type whose guard holds; the value's own
    /// barrier is a separate node.
    StoreBodySlot(otter_vm::jit::JitBodySlot),
    /// `[[Get]]` of the named property at the load site `pc` from receiver
    /// input0: an own or prototype data slot proved by the load fact in the
    /// isolate's shared property action table for `atom`, else the committed
    /// source operation in the runtime. A `length` site first reads an
    /// ordinary array's or a string's length inline.
    LoadPropertyCached {
        pc: u32,
        atom: Option<u32>,
        length: bool,
    },
    /// `[[Set]]` of input1 as the named property of receiver input0 at the
    /// store site `pc`: an existing writable own slot or an addition proved
    /// by the independent store fact in the shared property action table for
    /// `atom`; else the committed source operation in the runtime.
    /// The source receiver remains input0; each hit barriers its selected
    /// storage unless `barrier` is false: the value is never a cell, or the
    /// receiver is a fresh ordinary cell, which is its own storage.
    StorePropertyCached {
        pc: u32,
        atom: Option<u32>,
        barrier: bool,
    },
    /// `[[Set]]` of input2 at key input1 of receiver input0 at the element
    /// store site `pc` as a generic keyed store: an in-bounds store into
    /// an Array's dense storage of whatever kind it has, a hole included
    /// while no indexed accessor exists and the array has no exotic state;
    /// a name key through the isolate's shared property table; else the
    /// committed source operation in the runtime, which also grows storage.
    StoreKeyedCached {
        pc: u32,
    },
    /// `[[Get]]` of key input1 of receiver input0 at the element load site
    /// `pc` as a generic keyed load: an in-bounds element of an Array's
    /// dense storage of whatever kind it has, a name key through the
    /// isolate's shared property table, else the committed source operation
    /// in the runtime (holes, accessors, strings, typed arrays, proxies).
    LoadKeyedCached {
        pc: u32,
    },
    /// `input0.exec(input1)` (or `.test` when `test`) of the realm's
    /// original RegExp method, whose identity a preceding check proved: the
    /// method's complete semantics through its runtime entry, without a
    /// native call frame. May run code (a `lastIndex` coercion) and throw.
    RegExpMethod {
        test: bool,
    },
    /// Tagged word at `[input0 + offset]` (input0 a `Word` base or a tagged
    /// cell address).
    LoadTaggedField(i32),
    /// Store input1 (tagged) at `[input0 + offset]`.
    StoreTaggedField(i32),
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
    /// The `length` of the Array input0 as an int32. Eager deopt when input0
    /// is not an Array or its length exceeds the int32 range. A physical
    /// body proof skips only the receiver check, never the length-range check.
    LoadArrayLength {
        array_proved: bool,
    },
    /// The `length` of the typed view input0 as an int32: its element count
    /// while it owns no named property, its storage is a fixed-length local
    /// buffer and no buffer was ever detached. Eager deopt otherwise, or when
    /// the count exceeds the int32 range.
    LoadTypedArrayLength,
    /// The live element count of a proved indexed receiver input0, as a
    /// `Word`.
    LoadElementsLength {
        byte: u32,
        width: JitGuardWidth,
    },
    /// The element base of a proved receiver input0, a word at `byte` of its
    /// body. `off_heap` marks a typed view's cached base into a backing store
    /// the collector never moves; any other base names a movable slab.
    LoadElementsBase {
        byte: u32,
        off_heap: bool,
    },
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
    /// Exact two boxed operands of a pure declared native leaf. A pre-effect
    /// miss eagerly resumes this CallWithThis with its evaluated inputs.
    /// The C call clobbers registers but cannot collect or reenter.
    NativeLeaf(otter_vm::native_abi::RuntimeStubId),
    /// `[[Call]]` of input0 with `undefined` as receiver and the remaining
    /// inputs as arguments, through the JavaScript call ABI, or with
    /// `construct` its `[[Construct]]` with input0 as `new.target`. With a
    /// `plan`, a callee proved to be the plan's function is entered through
    /// its current generation; any other callee through the generic entry.
    /// Its actual return site names the source record while the callee runs.
    CallJs {
        pc: u32,
        plan: crate::call_linkage::CallPlan,
        construct: bool,
        /// Input1 is the receiver; otherwise the receiver is `undefined`.
        receiver: bool,
        /// The receiver is proved an Object: the callee is entered past a
        /// sloppy body's receiver test.
        receiver_object: bool,
        /// For a construct of a proven base constructor: how the receiver
        /// is allocated before the constructor is entered, so its prologue
        /// does not prepare one in the runtime.
        allocation: Option<otter_vm::jit::JitReceiverAllocationPlan>,
    },
    /// `[[Call]]` of input0 with input1 as receiver and the activation's own
    /// actual arguments, as `callee.apply(this, arguments)` passes them over
    /// an `arguments` object the body never materializes. The remaining
    /// inputs are the current values of the mapped formals at the argument
    /// indices `bindings`, which replace the actuals they alias. With a
    /// `plan`, a callee proved to be the plan's function is entered through
    /// its current generation; any other callee through the generic entry.
    CallForward {
        pc: u32,
        plan: crate::call_linkage::CallPlan,
        bindings: Box<[u16]>,
    },
    /// Eager deopt unless the callee of the call site at this byte PC is
    /// `%Function.prototype.call%`: input0 itself for an explicit-receiver
    /// call, or the `call` a method call reads from its closure receiver
    /// input0 through the pinned `%Function.prototype%`.
    CheckFunctionPrototypeCall(u32),
    /// Eager deopt unless input0 is the native function whose external
    /// reference index is the immediate.
    CheckNative(u32),
    /// Eager deopt once the activation has materialized its arguments
    /// object: its actual arguments are then that object's elements.
    CheckArgumentsElided,
    /// The physical activation's actual argument count, an int32: the
    /// elided `arguments.length`.
    LoadArgumentCount,
    /// The physical activation's actual argument at int32 input0: the
    /// elided `arguments[i]`. Eager deopt once an arguments object exists
    /// or out of range, where the interpreter's ordinary `[[Get]]` applies.
    LoadActualArgument,
    /// The inlined body's actual argument at int32 input0 among the
    /// `count` inputs after it: the elided `arguments[i]` of a spliced
    /// call. Eager deopt out of range.
    SelectArgument {
        count: u8,
    },
    /// Eager deopt unless input0 is the function `function_id`: its
    /// function-id immediate, or a closure of it that needs no runtime setup.
    /// `cell`, when not zero, holds the last value proved here, which a
    /// repeated callee matches with one compare.
    CheckFunction {
        function_id: u32,
        cell: u64,
    },
    /// Eager deopt unless input0 is the value the identity cell at this
    /// address holds; the collector keeps the cell's value current.
    CheckCellValue(u64),
    /// The value the code-retained identity cell at this address holds; the
    /// collector keeps the cell's value current.
    LoadCellValue(u64),
    /// Every closure of this source has the one context baked by the VM.
    CheckClosureContext(u32),
    /// The builder's proof that this slot of the context whose watch has this
    /// identity still holds the value the generation read as a constant. The
    /// generation subscribes to the slot; [`super::dependency_checks`] turns
    /// the node into a [`Kind::CheckDependencies`] or removes it before
    /// allocation.
    CheckContextSlot {
        watch: u32,
        slot: u16,
    },
    /// A store just wrote this slot of context input0: when the context's
    /// watch covers the slot, fire the slot's subscribers through the VM.
    NoteContextWrite(u16),
    /// Eager deopt when input0 is the hole of a binding still in its
    /// temporal dead zone.
    CheckNotHole,
    /// `input0 instanceof input1`: the prototype chain walk of the default
    /// `@@hasInstance` inline, every other case in the runtime.
    Instanceof,
    /// `input0.push(input1)` at the guarded method call at this byte PC,
    /// whose callee is `%Array.prototype.push%`: an append to tagged or
    /// absent storage of an Array whose dense prefix is its whole length
    /// inline, growing full storage into a nursery slab, every other Array
    /// state through the builtin in the runtime. Eager
    /// deopt unless the receiver is an Array that reads the builtin from its
    /// pinned prototype and no indexed accessor exists in the realm.
    ArrayPush(u32),
    /// `input0.pop()` at the guarded method call at this byte PC, whose
    /// callee is `%Array.prototype.pop%`: removal of a present tagged last
    /// element inline, every other Array state through the builtin in the
    /// runtime. Eager deopt unless the receiver is an Array that reads the
    /// builtin from its pinned prototype.
    ArrayPop(u32),
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
        /// The method is read from a code-retained cell the target's chain
        /// proof fixes, not from the holder's slot.
        cell_read: bool,
    },
    /// The compressed shape of input0 when it is an ordinary object that
    /// looks its properties up through its shape; zero otherwise.
    LoadReceiverShape,
    /// `ToNumber` of the tagged boolean input0 as an int32: 1 or 0.
    BooleanToInt32,
    /// The baseline operation for the bytecode instruction at `pc`, run on
    /// the frame window: input `i` is stored to window register
    /// `registers[i]` first, and the instruction's written registers are read
    /// back by `LoadWindow` nodes after it.
    Generic {
        pc: u32,
        registers: Box<[u16]>,
    },

    // ---- Back-edge countdown ----
    /// The shared back-edge countdown cell's value, as a `Word`.
    LoadBackedgeFuel,
    /// Input0 minus one: a loop's register countdown after one back edge.
    DecrementBackedgeFuel,
    /// Write the `Word` countdown input0 back to the shared cell.
    StoreBackedgeFuel,

    // ---- Control ----
    Jump(BlockId),
    /// Back edge to a loop header; carries the interrupt poll. With an
    /// input, the loop's back-edge countdown is that `Word` register, which
    /// the poll tests; otherwise the poll counts in the shared cell.
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
    /// The builder's proof, after an operation that may have run code, that
    /// every one of these shapes still has each object it ever had. The
    /// generation subscribes to their stability;
    /// [`super::dependency_checks`] turns the node into a
    /// [`Kind::CheckDependencies`] or removes it before allocation.
    CheckStableShapes(SmallVec<[u32; 4]>),
    /// Eager deopt when the generation's dependency flag is set: a shape fact
    /// it subscribed to has changed since installation.
    CheckDependencies,
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
    /// or run JavaScript: exact-live registers are preserved through canonical
    /// homes and the safepoint roots the initialized tagged region.
    pub(crate) may_collect: bool,
}

/// Where the allocator must put one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InputPolicy {
    /// Any general or floating-point register matching the input's repr.
    Register,
    /// Register, spill slot, or constant.
    Any,
    /// The value's canonical representation-specific home, or a rematerialized
    /// constant. Bulk consumers do not reserve an input register per value.
    Home,
    /// A constant stays one, for the instruction to encode; any other value
    /// is in a register.
    RegisterOrConstant,
    /// This general register.
    FixedGp(u8),
    /// Encode a constant directly; otherwise use this general register.
    /// A constant does not reserve or displace the named register.
    FixedGpOrConstant(u8),
}

/// Where the allocator must put the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResultPolicy {
    None,
    Register,
    FixedGp(u8),
}

/// Register-allocation contract of one node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Constraints {
    pub(crate) inputs: SmallVec<[InputPolicy; 4]>,
    pub(crate) result: ResultPolicy,
    pub(crate) gp_temps: u8,
    pub(crate) fp_temps: u8,
    /// Implicit writes by the operation, reserved and evicted before inputs
    /// are assigned. A fixed result may occupy one of these registers only
    /// after the operation's guards have consumed the preserved inputs.
    pub(crate) fixed_gp_clobbers: SmallVec<[u8; 2]>,
}

impl Kind {
    /// Whether the node's tagged result is always `true` or `false`.
    pub(crate) fn produces_boolean(&self) -> bool {
        matches!(
            self,
            Self::Int32Compare(_)
                | Self::PrimitiveCompare(_)
                | Self::Float64Compare(_)
                | Self::StrictEqual { .. }
                | Self::LooseEqual { .. }
                | Self::NullishEqual { .. }
                | Self::ToBoolean
                | Self::LogicalNot
                | Self::TestTypeOf { .. }
                | Self::Instanceof
        )
    }

    pub(crate) fn is_constant(&self) -> bool {
        matches!(
            self,
            Self::ConstTagged(_) | Self::ConstInt32(_) | Self::ConstFloat64(_)
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
            | Self::VirtualObject { .. }
            | Self::InitialRegister(_)
            | Self::LoadNewTarget
            | Self::LoadClosure
            | Self::LoadArgumentCount
            | Self::LoadLiteral(_)
            | Self::LoadCellValue(_)
            | Self::LoadWindow(_)
            | Self::Phi
            | Self::AllocationProjection(_)
            | Self::DecrementBackedgeFuel
            | Self::Int32AddWrapping
            | Self::Int32SubWrapping
            | Self::Int32MulExact
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
            | Self::StrictEqual { .. }
            | Self::NullishEqual { .. }
            | Self::ToBoolean
            | Self::LogicalNot
            | Self::TestTypeOf { .. }
            | Self::LoadOwnField { .. }
            | Self::LoadTaggedField(_)
            | Self::LoadContextParent
            | Self::LoadClosureContext
            | Self::LoadElementsLength { .. }
            | Self::LoadElementsBase { .. }
            | Self::LoadReceiverShape
            | Self::BooleanToInt32
            | Self::LoadElementUint32ToFloat64 => pure,
            Self::LoadElement(element) => {
                if matches!(element, JitElementRepr::Boxed | JitElementRepr::Uint32) {
                    eager
                } else {
                    pure
                }
            }
            Self::Float64Mod | Self::Float64Math(_) => pure,
            Self::Int32Add
            | Self::Int32Sub
            | Self::Int32Mul
            | Self::Int32MulIdentifyZeros
            | Self::Int32Div
            | Self::Int32Mod
            | Self::Int32Negate
            | Self::Int32ShiftRightLogical
            | Self::CheckedTaggedToInt32
            | Self::CheckedTaggedToFloat64
            | Self::CheckedFloat64ToInt32
            | Self::CheckedTaggedToIndex
            | Self::CheckedFloat64ToIndex
            | Self::CheckNumber
            | Self::CheckNotNullish { .. }
            | Self::CheckShapes { .. }
            | Self::CheckBounds
            | Self::CheckFunctionPrototypeCall(_)
            | Self::CheckNative(_)
            | Self::CheckArgumentsElided
            | Self::LoadActualArgument
            | Self::SelectArgument { .. }
            | Self::CheckFunction { .. }
            | Self::CheckCellValue(_)
            | Self::CheckNotHole
            | Self::LoadGuardedMethod { .. }
            | Self::LoadNamedProperty { .. }
            | Self::CheckElements { .. }
            | Self::CheckElementPresent
            | Self::LoadPresentElement
            | Self::CheckHoleyElementPresent(_)
            | Self::LoadHoleyFloat64Element(_)
            | Self::LoadArrayLength { .. }
            | Self::LoadTypedArrayLength
            | Self::StringCharCodeAt => eager,
            // A derived constructor changes its physical this binding at
            // super(); lexical capture must read the current binding here.
            Self::LoadThis => Properties {
                effectful: true,
                ..Properties::default()
            },
            Self::LoadArrayExpando(_) | Self::LoadFunctionBag => Properties {
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
            Self::LoadGlobalThis => eager,
            Self::LoadGlobalBinding(_) => Properties {
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
            Self::PrimitiveCompare(_) => eager,
            Self::AllocationGroup(_)
            | Self::PrimitiveAdd
            | Self::BigIntBinary(_)
            | Self::NativeNewContext(_)
            | Self::CopyContext
            | Self::NewClosure
            | Self::NewArrayWithLength
            | Self::NewReceiver(_) => Properties {
                eager_deopt: true,
                writes: true,
                effectful: true,
                may_collect: true,
                ..Properties::default()
            },
            Self::CheckReceiverPlan(_) => Properties {
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
            Self::StoreNamedProperty { .. } | Self::StoreBodySlot(_) => Properties {
                eager_deopt: true,
                writes: true,
                effectful: true,
                ..Properties::default()
            },
            Self::LoadPropertyCached { .. }
            | Self::StorePropertyCached { .. }
            | Self::StoreKeyedCached { .. }
            | Self::LoadKeyedCached { .. }
            | Self::RegExpMethod { .. }
            | Self::Instanceof
            | Self::ArrayPush(_)
            | Self::ArrayPop(_)
            | Self::LooseEqual { .. }
            | Self::NewObject
            | Self::NewArrayEmpty
            | Self::NewObjectLiteral { .. }
            | Self::NewArrayLiteral => Properties {
                eager_deopt: true,
                can_throw: true,
                writes: true,
                effectful: true,
                may_collect: true,
                ..Properties::default()
            },
            Self::StoreOwnField { .. }
            | Self::StoreTaggedField(_)
            | Self::StoreElement(_)
            | Self::WriteBarrier
            | Self::ElementWriteBarrier
            | Self::NoteContextWrite(_) => Properties {
                writes: true,
                effectful: true,
                ..Properties::default()
            },
            Self::NativeLeaf(_) => Properties {
                call: true,
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
            Self::CallJs { .. } | Self::CallForward { .. } | Self::Generic { .. } => Properties {
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
            Self::LoadBackedgeFuel | Self::StoreBackedgeFuel => Properties {
                effectful: true,
                ..Properties::default()
            },
            Self::Deopt(_)
            | Self::CheckStableShapes(_)
            | Self::CheckFieldTypes(_)
            | Self::CheckClosureContext(_)
            | Self::CheckContextSlot { .. }
            | Self::CheckDependencies => Properties {
                eager_deopt: true,
                effectful: true,
                ..Properties::default()
            },
        }
    }

    /// The allocation contract for a node with `input_count` inputs.
    pub(crate) fn constraints(
        &self,
        input_count: usize,
        target: &super::registers::RegisterContract,
    ) -> Constraints {
        use InputPolicy::{Any, FixedGp, FixedGpOrConstant, Register, RegisterOrConstant};
        let registers =
            |count: usize| -> SmallVec<[InputPolicy; 4]> { (0..count).map(|_| Register).collect() };
        let simple = |inputs: usize, result: ResultPolicy| Constraints {
            inputs: registers(inputs),
            result,
            gp_temps: 0,
            fp_temps: 0,
            fixed_gp_clobbers: SmallVec::new(),
        };
        match self {
            Self::ConstTagged(_)
            | Self::ConstInt32(_)
            | Self::ConstFloat64(_)
            | Self::Phi
            | Self::VirtualObject { .. } => Constraints {
                inputs: (0..input_count).map(|_| Any).collect(),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::PrimitiveAdd => Constraints {
                inputs: (0..input_count).map(|_| InputPolicy::Home).collect(),
                result: ResultPolicy::Register,
                gp_temps: 4,
                fp_temps: 1,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::BigIntBinary(_) => Constraints {
                inputs: (0..input_count).map(|_| InputPolicy::Home).collect(),
                result: ResultPolicy::Register,
                gp_temps: 5,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::PrimitiveCompare(_) => simple(2, ResultPolicy::Register),
            Self::AllocationProjection(_) => simple(1, ResultPolicy::Register),
            Self::AllocationGroup(_) | Self::NewObject | Self::NewArrayEmpty => Constraints {
                inputs: SmallVec::new(),
                result: ResultPolicy::Register,
                gp_temps: 4,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::NewObjectLiteral { .. }
            | Self::NewArrayLiteral
            | Self::NativeNewContext(_)
            | Self::CopyContext
            | Self::NewArrayWithLength
            | Self::NewClosure => Constraints {
                inputs: (0..input_count).map(|_| InputPolicy::Home).collect(),
                result: ResultPolicy::Register,
                gp_temps: 4,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::NewReceiver(_) => {
                let fixed = target.receiver_allocation;
                Constraints {
                    inputs: smallvec::smallvec![FixedGp(fixed.new_target)],
                    result: ResultPolicy::FixedGp(fixed.result),
                    gp_temps: 0,
                    fp_temps: 0,
                    fixed_gp_clobbers: fixed.clobbers.iter().copied().collect(),
                }
            }
            Self::CheckReceiverPlan(_) => {
                let fixed = target.receiver_allocation;
                Constraints {
                    inputs: smallvec::smallvec![FixedGp(fixed.new_target)],
                    result: ResultPolicy::None,
                    gp_temps: 0,
                    fp_temps: 0,
                    fixed_gp_clobbers: fixed.clobbers.iter().copied().collect(),
                }
            }
            Self::LoadGlobalBinding(_) => Constraints {
                inputs: SmallVec::new(),
                result: ResultPolicy::Register,
                gp_temps: 2,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::InitialRegister(_)
            | Self::LoadThis
            | Self::LoadNewTarget
            | Self::LoadClosure
            | Self::LoadLiteral(_)
            | Self::LoadCellValue(_)
            | Self::LoadGlobalThis
            | Self::LoadWindow(_) => simple(0, ResultPolicy::Register),
            // The right operand may be a constant, encoded in the
            // instruction when it fits.
            Self::Int32Add
            | Self::Int32Sub
            | Self::Int32AddWrapping
            | Self::Int32SubWrapping
            | Self::Int32BitAnd
            | Self::Int32BitOr
            | Self::Int32BitXor
            | Self::Int32Compare(_) => Constraints {
                inputs: smallvec::smallvec![Register, RegisterOrConstant],
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::Int32ShiftLeft | Self::Int32ShiftRight | Self::Int32ShiftRightLogical => {
                Constraints {
                    inputs: smallvec::smallvec![
                        Register,
                        target
                            .variable_shift_count
                            .map_or(RegisterOrConstant, FixedGpOrConstant)
                    ],
                    result: ResultPolicy::Register,
                    gp_temps: 0,
                    fp_temps: 0,
                    fixed_gp_clobbers: SmallVec::new(),
                }
            }
            Self::Uint32ShiftRightToFloat64 => Constraints {
                // ARM consumes a register even for a constant count.
                inputs: smallvec::smallvec![
                    Register,
                    target
                        .variable_shift_count
                        .map_or(Register, FixedGpOrConstant)
                ],
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::Int32Mul
            | Self::Int32MulExact
            | Self::Int32MulIdentifyZeros
            | Self::Float64Add
            | Self::Float64Sub
            | Self::Float64Mul
            | Self::Float64Div
            | Self::Float64Compare(_)
            | Self::LoadElement(_)
            | Self::LoadPresentElement
            | Self::LoadElementUint32ToFloat64 => simple(2, ResultPolicy::Register),
            Self::LoadHoleyFloat64Element(_) => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 1,
                fixed_gp_clobbers: SmallVec::new(),
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
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::CheckFunction { .. }
            | Self::CheckCellValue(_)
            | Self::CheckNotHole
            | Self::CheckNative(_) => simple(1, ResultPolicy::None),
            Self::CheckArgumentsElided => simple(0, ResultPolicy::None),
            Self::LoadArgumentCount => simple(0, ResultPolicy::Register),
            Self::LoadActualArgument => simple(1, ResultPolicy::Register),
            Self::SelectArgument { count } => {
                simple(1 + usize::from(*count), ResultPolicy::Register)
            }
            Self::LoadGuardedMethod { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 1,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::LoadArrayExpando(_) | Self::LoadFunctionBag => simple(1, ResultPolicy::Register),
            Self::StringCharCodeAt => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 1,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::LoadNamedProperty { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 2,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::StoreNamedProperty { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::None,
                gp_temps: 2,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::StoreBodySlot(_) => simple(2, ResultPolicy::None),
            Self::Instanceof => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 5,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::ArrayPush(_) | Self::ArrayPop(_) => Constraints {
                inputs: registers(input_count),
                result: ResultPolicy::Register,
                // An append that grows its storage carves the new slab with
                // three more.
                gp_temps: if matches!(self, Self::ArrayPush(_)) {
                    6
                } else {
                    3
                },
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::LoadPropertyCached { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 4,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::StorePropertyCached { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::None,
                gp_temps: 4,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::StoreKeyedCached { .. } => Constraints {
                inputs: registers(3),
                result: ResultPolicy::None,
                gp_temps: 2,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::LoadKeyedCached { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 2,
                fp_temps: 1,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::RegExpMethod { .. } => simple(2, ResultPolicy::Register),
            Self::Int32Div | Self::Int32Mod => {
                if let Some(pair) = target.integer_division {
                    Constraints {
                        // Both original operands remain readable by eager
                        // guards while implicit division writes its pair.
                        inputs: registers(2),
                        result: ResultPolicy::FixedGp(if matches!(self, Self::Int32Div) {
                            pair.quotient
                        } else {
                            pair.remainder
                        }),
                        gp_temps: 0,
                        fp_temps: 0,
                        fixed_gp_clobbers: smallvec::smallvec![pair.quotient, pair.remainder],
                    }
                } else {
                    Constraints {
                        inputs: registers(2),
                        result: ResultPolicy::Register,
                        gp_temps: 1,
                        fp_temps: 0,
                        fixed_gp_clobbers: SmallVec::new(),
                    }
                }
            }
            Self::StrictEqual { .. } | Self::LooseEqual { .. } => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 1,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::NullishEqual { .. } => simple(1, ResultPolicy::Register),
            Self::TestTypeOf { .. } => Constraints {
                inputs: registers(1),
                result: ResultPolicy::Register,
                gp_temps: 1,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::Float64Mod => Constraints {
                inputs: registers(2),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::Float64Math(op) => Constraints {
                inputs: registers(op.arity()),
                result: ResultPolicy::Register,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
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
            | Self::LoadOwnField { .. }
            | Self::LoadTaggedField(_)
            | Self::LoadContextParent
            | Self::LoadClosureContext
            | Self::LoadElementsLength { .. }
            | Self::LoadElementsBase { .. }
            | Self::LoadReceiverShape
            | Self::LoadArrayLength { .. }
            | Self::LoadTypedArrayLength
            | Self::BooleanToInt32 => simple(1, ResultPolicy::Register),
            Self::CheckedTaggedToFloat64 | Self::Float64ToTagged | Self::CheckedTaggedToIndex => {
                Constraints {
                    inputs: registers(1),
                    result: ResultPolicy::Register,
                    gp_temps: 0,
                    fp_temps: 1,
                    fixed_gp_clobbers: SmallVec::new(),
                }
            }
            Self::ToBoolean | Self::LogicalNot => simple(1, ResultPolicy::Register),
            Self::CheckNumber => simple(1, ResultPolicy::None),
            Self::CheckNotNullish { .. } => simple(1, ResultPolicy::None),
            Self::CheckShapes { .. } | Self::CheckElements { .. } => simple(1, ResultPolicy::None),
            Self::StoreOwnField { owners, .. } => Constraints {
                gp_temps: u8::from(!owners.is_empty()),
                ..simple(2, ResultPolicy::None)
            },
            Self::StoreTaggedField(_) => simple(2, ResultPolicy::None),
            Self::WriteBarrier => simple(2, ResultPolicy::None),
            Self::NoteContextWrite(_) => simple(1, ResultPolicy::None),
            Self::NativeLeaf(_) => Constraints {
                inputs: (0..input_count).map(|_| InputPolicy::Home).collect(),
                result: ResultPolicy::FixedGp(target.call_result),
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::CallJs { receiver, .. } => {
                let mut inputs: SmallVec<[InputPolicy; 4]> =
                    smallvec::smallvec![FixedGp(target.call_callee)];
                if *receiver {
                    inputs.push(FixedGp(target.call_receiver));
                }
                inputs.extend((inputs.len()..input_count).map(|_| Any));
                Constraints {
                    inputs,
                    result: ResultPolicy::FixedGp(target.call_result),
                    gp_temps: 0,
                    fp_temps: 0,
                    fixed_gp_clobbers: SmallVec::new(),
                }
            }
            Self::CallForward { .. } => {
                let mut inputs: SmallVec<[InputPolicy; 4]> =
                    smallvec::smallvec![FixedGp(target.call_callee), FixedGp(target.call_receiver)];
                inputs.extend((inputs.len()..input_count).map(|_| Any));
                // The old stack pointer and the actual span's source survive
                // the copy that fills the span.
                Constraints {
                    inputs,
                    result: ResultPolicy::FixedGp(target.call_result),
                    gp_temps: 2,
                    fp_temps: 0,
                    fixed_gp_clobbers: SmallVec::new(),
                }
            }
            Self::Generic { .. } => Constraints {
                inputs: (0..input_count).map(|_| Any).collect(),
                result: ResultPolicy::FixedGp(target.call_result),
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::JumpLoop(_) => simple(input_count, ResultPolicy::None),
            Self::LoadBackedgeFuel => simple(0, ResultPolicy::Register),
            Self::DecrementBackedgeFuel => simple(1, ResultPolicy::Register),
            Self::StoreBackedgeFuel => simple(1, ResultPolicy::None),
            Self::Jump(_)
            | Self::Deopt(_)
            | Self::CheckStableShapes(_)
            | Self::CheckFieldTypes(_)
            | Self::CheckContextSlot { .. }
            | Self::CheckClosureContext(_)
            | Self::CheckDependencies => simple(0, ResultPolicy::None),
            Self::Branch {
                kind: BranchKind::Int32(_),
                ..
            } => Constraints {
                inputs: smallvec::smallvec![Register, RegisterOrConstant],
                result: ResultPolicy::None,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
            },
            Self::Branch { .. } => simple(input_count, ResultPolicy::None),
            Self::Return => Constraints {
                inputs: smallvec::smallvec![FixedGp(target.call_result)],
                result: ResultPolicy::None,
                gp_temps: 0,
                fp_temps: 0,
                fixed_gp_clobbers: SmallVec::new(),
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
            for argument in caller.arguments.iter_mut().flatten() {
                f(argument);
            }
        }
        for (_, value) in &mut self.registers {
            f(value);
        }
    }
}

/// The call an inlined frame runs for.
#[derive(Debug, Clone)]
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
    /// The call's actual arguments when legacy `fn.arguments` can read the
    /// frame or its body reads `arguments`; the rebuilt activation's
    /// incoming arguments.
    pub(crate) arguments: Option<SmallVec<[NodeId; 4]>>,
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

/// Every frame state's [`Graph::state_values`], in state order.
#[derive(Debug, Default)]
pub(crate) struct StateValues {
    /// Start of each state's values; one more entry closes the last.
    starts: Vec<u32>,
    values: Vec<NodeId>,
}

impl StateValues {
    /// The values `state`'s chain reads, outermost frame first.
    pub(crate) fn of(&self, state: FrameStateId) -> &[NodeId] {
        let index = state.0 as usize;
        &self.values[self.starts[index] as usize..self.starts[index + 1] as usize]
    }
}

/// The whole graph of one compilation.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    pub(crate) nodes: Vec<Node>,
    pub(crate) allocation_groups: Vec<super::allocation_groups::Group>,
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
            cursor = self
                .frame_state(state)
                .caller
                .as_ref()
                .map(|caller| caller.state);
        }
        chain.reverse();
        chain
    }

    /// Every value the frame states of `id`'s chain read, outermost frame
    /// first: an inlined frame's entry bindings (`this`, closure,
    /// `new.target`, recorded actual arguments), then each frame's live
    /// registers. A [`Kind::VirtualObject`] reads its fields in its place, at
    /// its first occurrence in the chain only; later occurrences read
    /// nothing.
    pub(crate) fn state_values(&self, id: FrameStateId) -> SmallVec<[NodeId; 16]> {
        let mut values = SmallVec::new();
        let mut expanded: SmallVec<[NodeId; 4]> = SmallVec::new();
        for state in self.state_chain(id) {
            let data = self.frame_state(state);
            if let Some(caller) = &data.caller {
                for value in [caller.this, caller.closure, caller.new_target]
                    .into_iter()
                    .chain(caller.arguments.iter().flatten().copied())
                {
                    self.push_state_value(value, &mut values, &mut expanded);
                }
            }
            for &(_, value) in &data.registers {
                self.push_state_value(value, &mut values, &mut expanded);
            }
        }
        values
    }

    /// Append `value` to a state's values, or a virtual object's fields at
    /// its first occurrence.
    fn push_state_value(
        &self,
        value: NodeId,
        values: &mut SmallVec<[NodeId; 16]>,
        expanded: &mut SmallVec<[NodeId; 4]>,
    ) {
        if !matches!(self.node(value).kind, Kind::VirtualObject { .. }) {
            values.push(value);
            return;
        }
        if expanded.contains(&value) {
            return;
        }
        expanded.push(value);
        for &field in &self.node(value).inputs {
            self.push_state_value(field, values, expanded);
        }
    }

    /// [`Self::state_values`] of every frame state, flattened once the graph
    /// is final.
    pub(crate) fn all_state_values(&self) -> StateValues {
        let mut starts = Vec::with_capacity(self.frame_states.len() + 1);
        let mut values = Vec::new();
        for state in 0..self.frame_states.len() {
            starts.push(values.len() as u32);
            values.extend(self.state_values(FrameStateId(state as u32)));
        }
        starts.push(values.len() as u32);
        StateValues { starts, values }
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
}
