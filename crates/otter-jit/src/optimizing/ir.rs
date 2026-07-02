//! Typed SSA graph for the optimizing tier.
//!
//! A function in the optimizing subset is represented as a control-flow graph
//! of basic [`Block`]s over a flat arena of SSA [`Node`]s. Every node carries a
//! machine [`Repr`] (`Tagged` / `Int32` / `Float64` / `Bool`); arithmetic and
//! comparison nodes are produced in a typed form (`Int32Add`, `Float64Mul`,
//! `Int32Compare`, …) guarded by `Check*` speculation nodes inserted from the
//! interpreter's operand-type feedback. The graph is the input to SSA liveness,
//! linear-scan register allocation, deopt frame-state capture, and arm64
//! lowering; on its own it emits no code.
//!
//! # Contents
//! - [`Repr`] — machine representation lattice element.
//! - [`NodeKind`] / [`Node`] — typed SSA operations.
//! - [`Terminator`] — per-block control transfer.
//! - [`Block`] / [`Graph`] — the CFG and node arena.
//!
//! # Invariants
//! - **SSA.** Every node is assigned exactly once. Register-level mutability of
//!   the source bytecode is resolved to `Phi` nodes during construction
//!   ([`super::builder`]).
//! - **Register values are tagged at block boundaries.** A `Phi` always has
//!   [`Repr::Tagged`]; typed results (`Int32Add`, …) that flow into a phi are
//!   boxed at the predecessor edge by the lowering pass. Typed islands are
//!   bounded by `Check*` (unbox on read) and box-on-cross-block.
//! - **Speculation is explicit.** A typed arithmetic node only ever consumes
//!   the result of a matching `Check*` (or another node already in that repr);
//!   a failed `Check*` deoptimizes to the interpreter at the guard's exact PC.
//!
//! # See also
//! - [`super::builder`] — bytecode → SSA construction.
//! - [`crate::optimizing`] — tier entry and `Unsupported` reasons.

use otter_bytecode::Op;

/// Dense index into [`Graph::nodes`].
pub type NodeId = u32;
/// Dense index into [`Graph::blocks`].
pub type BlockId = u32;

/// Machine representation an SSA value is materialized in.
///
/// `Tagged` is the NaN-boxed `Value` every register slot holds at a block
/// boundary; `Int32` and `Float64` are unboxed numeric islands; `Bool` is an
/// unboxed branch predicate produced by a comparison.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Repr {
    /// NaN-boxed `Value` (the universal register representation).
    Tagged,
    /// Unboxed 32-bit signed integer (low 32 bits of a GP register).
    Int32,
    /// Unboxed IEEE-754 `f64` (an FP register / spill slot). A boxed double is
    /// its bits verbatim, so boxing a `Float64` is the identity bit-pattern move
    /// from the FP home into a GP register.
    Float64,
    /// Unboxed branch predicate.
    Bool,
}

/// Relational / equality comparison kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpOp {
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
    /// `==` / `===` (numeric operands only in this tier)
    Eq,
    /// `!=` / `!==`
    Ne,
}

impl CmpOp {
    /// Map a relational / equality bytecode opcode to its [`CmpOp`], or `None`
    /// for a non-comparison opcode.
    #[must_use]
    pub fn from_op(op: Op) -> Option<Self> {
        Some(match op {
            Op::LessThan => Self::Lt,
            Op::LessEq => Self::Le,
            Op::GreaterThan => Self::Gt,
            Op::GreaterEq => Self::Ge,
            Op::Equal => Self::Eq,
            Op::NotEqual => Self::Ne,
            _ => return None,
        })
    }
}

/// An exactly-mappable `Math.*` unary, each a single aarch64 float instruction
/// whose IEEE result matches the JS spec for every input. Functions whose
/// rounding differs (`Math.round` ties toward `+Inf`, not to even) or that need
/// a libm call are deliberately excluded — those decline to the baseline.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Float64UnaryOp {
    /// `Math.sqrt` → `fsqrt`.
    Sqrt,
    /// `Math.abs` → `fabs`.
    Abs,
    /// `Math.floor` → `frintm` (round toward −∞).
    Floor,
    /// `Math.ceil` → `frintp` (round toward +∞).
    Ceil,
    /// `Math.trunc` → `frintz` (round toward zero).
    Trunc,
}

/// A typed SSA operation.
#[derive(Clone, Debug)]
pub enum NodeKind {
    /// Initial value of source register `n` on function entry (a parameter, or
    /// `undefined` for an uninitialized local — see the builder).
    Param(u16),
    /// Unboxed `int32` literal.
    ConstInt32(i32),
    /// Unboxed `f64` literal (a `LoadNumber` constant). Result [`Repr::Float64`].
    ConstF64(f64),
    /// Boxed boolean literal.
    ConstBool(bool),
    /// Boxed `undefined`.
    ConstUndefined,
    /// Boxed `null`.
    ConstNull,
    /// The running function's own closure (`MakeFunction` self-binding), read
    /// from the entry context. A tagged value never used as a numeric operand in
    /// this subset (a use that needs it as a number deoptimizes); present so the
    /// near-universal leading self-binding does not disqualify a function.
    SelfClosure,
    /// SSA phi. Operands are aligned 1:1 with the owning block's `preds`. May be
    /// temporarily empty (incomplete) during construction of an unsealed block.
    Phi(Vec<NodeId>),
    /// Speculative "operand is int32" guard; result repr [`Repr::Int32`]. A
    /// non-int32 input deoptimizes.
    CheckInt32(NodeId),
    /// `int32 + int32`, deopt on overflow. Result [`Repr::Int32`].
    Int32Add(NodeId, NodeId),
    /// `int32 - int32`, deopt on overflow.
    Int32Sub(NodeId, NodeId),
    /// `int32 * int32`, deopt on overflow.
    Int32Mul(NodeId, NodeId),
    /// `int32 <cmp> int32`. Result [`Repr::Bool`].
    Int32Compare(CmpOp, NodeId, NodeId),
    /// Speculative "operand is a number" guard; result repr [`Repr::Float64`]
    /// (the operand unboxed to an `f64`). An int32-tagged input is widened to a
    /// double; a real double is unboxed verbatim; a non-number input
    /// deoptimizes.
    CheckNumber(NodeId),
    /// Speculative "operand is a boxed boolean" guard; result repr [`Repr::Bool`].
    /// A boxed `false` becomes `0`, boxed `true` becomes `1`, and any other
    /// value deoptimizes so the interpreter owns full JavaScript truthiness.
    CheckBool(NodeId),
    /// Widen an unboxed `int32` to `f64` (arm64 `scvtf`). Used to bring an
    /// int32-typed operand into a `Float64` arithmetic site without a guard.
    Int32ToFloat64(NodeId),
    /// JavaScript `ToInt32` on an unboxed `f64`. Used by bitwise / shift
    /// operators after arithmetic widening: unlike `CheckInt32`, this is a
    /// coercion and is total for every number (`NaN` / infinities become `0`,
    /// finite values truncate and wrap modulo 2^32). Result [`Repr::Int32`].
    Float64ToInt32(NodeId),
    /// `f64 + f64` (no overflow — IEEE arithmetic is total). Result
    /// [`Repr::Float64`].
    Float64Add(NodeId, NodeId),
    /// `f64 - f64`.
    Float64Sub(NodeId, NodeId),
    /// `f64 * f64`.
    Float64Mul(NodeId, NodeId),
    /// `f64 / f64`.
    Float64Div(NodeId, NodeId),
    /// `f64 <cmp> f64`. Result [`Repr::Bool`]. IEEE ordered comparison
    /// (a `NaN` operand yields `false` for every relation, matching JS).
    Float64Compare(CmpOp, NodeId, NodeId),
    /// Allocate an array literal by materializing the frame and delegating to the
    /// VM's rooted `NewArray` path for the bytecode instruction. The runtime
    /// helper decodes the source registers from the bytecode and writes the
    /// result to the instruction's destination frame slot. Result
    /// [`Repr::Tagged`].
    NewArray,
    /// Nullish identity check against the boxed `null` (and, when `nullish`,
    /// `undefined`) immediate. With `nullish=false` this is strict `value ===
    /// null` (`negate=true` → `!== null`). With `nullish=true` it matches `null`
    /// OR `undefined`, the speculative lowering of loose `value == null` /
    /// `value != null` — sound because `x == null` is true iff `x` is one of the
    /// two nullish values. `null` and `undefined` box to `base|1` and `base|0`
    /// (differing only in bit 0), so the emitter masks bit 0 before comparing.
    TaggedIsNull {
        /// Tagged value to compare with `null` (and `undefined` when `nullish`).
        value: NodeId,
        /// Invert the predicate for `!== null` / `!= null`.
        negate: bool,
        /// Match `undefined` as well as `null` (loose equality against a nullish
        /// literal). `false` keeps the strict null-only identity check.
        nullish: bool,
    },
    /// `int32 | int32`. Result [`Repr::Int32`]. Total (no deopt).
    Int32BitOr(NodeId, NodeId),
    /// `int32 & int32`.
    Int32BitAnd(NodeId, NodeId),
    /// `int32 ^ int32`.
    Int32BitXor(NodeId, NodeId),
    /// `int32 << (int32 & 31)` — arm64 32-bit `lslv` masks the shift amount mod
    /// 32, matching JS `ToInt32(a) << (ToUint32(b) & 31)`. Result wraps to int32.
    Int32Shl(NodeId, NodeId),
    /// `int32 >> (int32 & 31)` — arithmetic (sign-propagating) right shift,
    /// matching JS `>>`. Result [`Repr::Int32`].
    Int32Shr(NodeId, NodeId),
    /// `int32 >>> (int32 & 31)` — logical right shift, then widened to `f64`
    /// because JavaScript `>>>` returns an unsigned 32-bit value that may not
    /// fit signed int32. Result [`Repr::Float64`].
    Int32UshrToFloat64(NodeId, NodeId),
    /// The function's `this` binding, read from `JitCtx.this_value`. A TDZ hole
    /// (a derived-constructor `this` before `super(...)`) deoptimizes — the
    /// interpreter owns that ReferenceError. Result [`Repr::Tagged`].
    LoadThis,
    /// The TDZ / uninitialized hole sentinel. Result [`Repr::Tagged`].
    LoadHole,
    /// Load a captured binding from the current frame's upvalue spine. The
    /// upvalue index is the bytecode immediate. Missing spine / TDZ hole
    /// deoptimizes so the interpreter owns the exact ReferenceError path.
    /// Result [`Repr::Tagged`].
    LoadUpvalue(i32),
    /// Direct bytecode function call. Inputs are `(callee, args...)`; result is
    /// the boxed return value. The emitter materializes a call safepoint frame,
    /// prepares a compiled direct callee through the VM, and deoptimizes at the
    /// call PC when the site is ineligible.
    Call {
        /// Bytecode register containing the callee value.
        callee_reg: u16,
        /// Bytecode argument registers, in call order.
        arg_regs: Vec<u16>,
        /// SSA inputs `(callee, args...)`, matching the register metadata above.
        inputs: Vec<NodeId>,
    },
    /// Guard that a direct-call callee still denotes the monomorphic bytecode
    /// function whose body was inlined at the call site. Result is the original
    /// tagged callee value; the inlined body itself does not consume it, but the
    /// guard pins the speculative dependency and owns deopt at the call PC.
    CheckFunctionIdentity {
        /// Callee value observed at the call site.
        callee: NodeId,
        /// Expected bytecode function id.
        function_id: u32,
    },
    /// Bytecode method call. Inputs are `(receiver, args...)`; the property name
    /// and IC site are bytecode metadata. A monomorphic compiled method uses the
    /// direct-call protocol; every ineligible receiver/method falls back to the
    /// full method-call VM stub in place.
    CallMethod {
        /// Receiver object/value.
        recv: NodeId,
        /// Bytecode receiver register.
        recv_reg: u16,
        /// Property atom index.
        name: u32,
        /// Monomorphic call IC site index.
        site: u64,
        /// Bytecode argument registers, in call order.
        arg_regs: Vec<u16>,
        /// Argument values.
        args: Vec<NodeId>,
    },
    /// Guard that `recv.name` still resolves to the monomorphic bytecode method
    /// whose body was inlined at a `CallMethodValue` site. Result is the tagged
    /// receiver, so the inlined body can use the checked value as `this`.
    CheckMethodIdentity {
        /// Receiver object/value.
        recv: NodeId,
        /// Receiver shape-handle compressed offset.
        recv_shape: u32,
        /// Prototype shape-handle compressed offset.
        proto_shape: u32,
        /// Byte offset of the method slot in the prototype value slab.
        method_value_byte: u32,
        /// Whether the method slot is an own property on the receiver.
        method_on_receiver: bool,
        /// Expected bytecode function id.
        method_fid: u32,
    },
    /// Polymorphic-dispatch predicate: `true` when `recv` still resolves `name`
    /// to the specific bytecode method whose body is inlined on the matching arm
    /// of a polymorphic `CallMethodValue` guard chain. Unlike
    /// [`NodeKind::CheckMethodIdentity`] (which deoptimizes on a miss), this
    /// produces a [`Repr::Bool`] a [`Terminator::Branch`] consumes to fall
    /// through to the next candidate shape; the chain's final miss takes the
    /// method bridge, so no single shape is speculated. Mirrors JSC
    /// `ByteCodeParser::handleInlining`'s per-target `SwitchCell` and V8 Maglev's
    /// polymorphic call dispatch.
    MethodIdentityMatches {
        /// Receiver object/value.
        recv: NodeId,
        /// Receiver shape-handle compressed offset.
        recv_shape: u32,
        /// Prototype shape-handle compressed offset.
        proto_shape: u32,
        /// Byte offset of the method slot in the prototype value slab.
        method_value_byte: u32,
        /// Whether the method slot is an own property on the receiver.
        method_on_receiver: bool,
        /// Expected bytecode function id.
        method_fid: u32,
    },
    /// Speculative "operand is an ordinary object of the baked shape" guard.
    /// Carries the receiver and the receiver shape's compressed `Gc` offset. A
    /// non-object, or a different shape (or dictionary mode), deoptimizes.
    /// Result [`Repr::Tagged`] — the guarded receiver, passed through so a
    /// `LoadSlot` consumes it.
    CheckShape(NodeId, u32),
    /// Load the `Value` at a fixed byte offset within a shape-guarded receiver's
    /// value slab (the offset baked from the monomorphic own-data property IC).
    /// Pure (no deopt, no allocation); the preceding `CheckShape` established the
    /// receiver shape. Result [`Repr::Tagged`].
    LoadSlot(NodeId, u32),
    /// Direct-prototype data-property load. Input is the receiver; the baked
    /// metadata carries receiver shape, prototype shape, and the prototype slot
    /// byte offset. Lowered as object/prototype guards followed by an inline
    /// prototype slab load; any miss deoptimizes at the load's exact PC. Result
    /// [`Repr::Tagged`].
    LoadProtoSlot {
        /// Receiver object/value.
        recv: NodeId,
        /// Expected receiver shape-handle compressed offset.
        recv_shape: u32,
        /// Expected direct-prototype shape-handle compressed offset.
        proto_shape: u32,
        /// Byte offset of the property slot in the prototype value slab.
        slot_byte: u32,
    },
    /// Store a value into a fixed byte offset within a shape-guarded receiver's
    /// value slab. Inputs `(receiver, value)`; the value is always a primitive
    /// (int32 / f64 / bool), so the stored `Value` is never a `Gc` pointer and no
    /// generational write barrier is needed. A side effect — produces no result.
    StoreSlot(NodeId, u32, NodeId),
    /// Polymorphic own-data slot load (a JSC `MultiGetByOffset`). Input is the
    /// receiver; the boxed list carries one `(shape_offset, slot_byte)` case per
    /// observed receiver shape. Lowered as an inline structure-guard chain: each
    /// case compares the receiver shape and, on a match, loads the slot at its
    /// offset and writes the destination; the final miss deoptimizes at the load's
    /// exact PC. Result [`Repr::Tagged`].
    LoadSlotPoly(NodeId, Box<[(u32, u32)]>),
    /// Polymorphic own-data slot store (a JSC `MultiPutByOffset`). Inputs are
    /// `(receiver, value)`; the boxed list carries one `(shape_offset, slot_byte)`
    /// case per observed receiver shape. Lowered as an inline structure-guard
    /// chain that stores the value at the matching case's offset (with the
    /// generational card-mark for a tagged value); the final miss deoptimizes at
    /// the store's exact PC before any write. Side effect — produces no result.
    StoreSlotPoly(NodeId, Box<[(u32, u32)]>, NodeId),
    /// Speculative dense-array / typed-array computed element read. Inputs are
    /// `(receiver, index)`, where `index` is already unboxed int32. A miss
    /// deoptimizes at the load's exact PC so the interpreter owns the full
    /// `[[Get]]` semantics. Result [`Repr::Tagged`].
    LoadElement(NodeId, NodeId),
    /// Speculative typed-array computed element write. Inputs are `(receiver,
    /// index, value)`, where `index` is already unboxed int32 and `value` is a
    /// primitive numeric value. A miss deoptimizes at the store's exact PC so the
    /// interpreter owns `[[Set]]`, coercion, accessors, growth, and barriers.
    /// Side-effect only; no result is consumed.
    StoreElement(NodeId, NodeId, NodeId),
    /// Speculative Array `.length` read. The receiver must be a dense Array body
    /// and the length must fit int32; otherwise deopt. Result [`Repr::Int32`].
    LoadArrayLength(NodeId),
    /// A `Math.*` unary call whose result is an exact single-instruction float
    /// operation (`Op::MathCall` with one argument already widened to `f64`).
    /// Total — `NaN` / `±Inf` / `±0` propagate per IEEE, matching the JS spec —
    /// so no deopt of its own; the operand's widening guard owns non-number
    /// inputs. Result [`Repr::Float64`].
    Float64Unary(Float64UnaryOp, NodeId),
    /// Allocate an object literal (`NewObject` + a source-order run of
    /// `DefineDataProperty` with constant string keys) directly in its final
    /// hidden class. `shape_offset` is the compressed `Gc` offset of the shape
    /// the literal's object ends up in after all properties are defined (baked
    /// by replaying the shape transitions at compile time). `inputs` are the SSA
    /// property values in slot order. The emitter materializes a call safepoint
    /// (the allocation can GC), boxes each value, and calls a runtime helper that
    /// allocates the shaped object, bulk-initializes its slots (write barriers in
    /// Rust), and installs `%Object.prototype%`. Result [`Repr::Tagged`] — the
    /// new object. Throws only on OOM (propagated like a `Call`).
    AllocObjectLiteral {
        /// Final hidden-class shape, as a compressed `Gc<ShapeBody>` offset.
        shape_offset: u32,
        /// Property values in slot (source-definition) order.
        inputs: Vec<NodeId>,
    },
    /// Read upvalue `index` from an *inlined closure callee's* own spine, rather
    /// than the running function's context spine (that is [`LoadUpvalue`]). The
    /// `closure` input is the call-site callee value the surrounding
    /// [`CheckFunctionIdentity`] pinned to a single bytecode function id; the
    /// emitter decodes the live closure body each time and reads the captured
    /// cell, so any closure of that id (whatever it captured) loads correctly
    /// without baking a GC pointer. Missing spine / TDZ hole deoptimizes. Result
    /// [`Repr::Tagged`].
    InlineUpvalue {
        /// Call-site callee value (a fid-guarded closure).
        closure: NodeId,
        /// Upvalue index within the closure's own spine.
        index: u32,
    },
    /// Speculative dense-array `pop()`. The receiver must be an ordinary dense
    /// array whose `%Array.prototype%` pop slot still holds the original builtin;
    /// any miss deoptimizes at the call PC so the interpreter owns the full
    /// semantics. Leaf — the only mutation shrinks the dense length, so no
    /// allocation, safepoint, or write barrier is needed and the popped value is
    /// the result (rooted in the destination). Result [`Repr::Tagged`].
    ArrayPop {
        /// Receiver array value.
        recv: NodeId,
    },
    /// Speculative dense-array `push(value)`. The receiver must be an ordinary
    /// dense array whose `%Array.prototype%` push slot still holds the original
    /// builtin; any miss deoptimizes at the call PC. The append can grow the
    /// backing store and store a heap pointer, so the emitter materializes a call
    /// safepoint and routes the push through a rooted runtime stub (growth and
    /// the generational barrier handled in Rust). Result is the new length
    /// [`Repr::Int32`].
    ArrayPush {
        /// Receiver array value.
        recv: NodeId,
        /// Value to append.
        value: NodeId,
        /// Bytecode receiver register (read back from the materialized frame).
        recv_reg: u16,
    },
}

impl NodeKind {
    /// SSA value operands this node consumes, in operand order. For a `Phi`
    /// these are the per-predecessor inputs (aligned to the block's `preds`),
    /// which liveness treats as uses at the predecessor edges rather than as
    /// block-local uses.
    #[must_use]
    pub fn inputs(&self) -> Vec<NodeId> {
        match self {
            NodeKind::CheckInt32(a)
            | NodeKind::CheckNumber(a)
            | NodeKind::CheckBool(a)
            | NodeKind::Int32ToFloat64(a)
            | NodeKind::Float64ToInt32(a)
            | NodeKind::TaggedIsNull { value: a, .. }
            | NodeKind::CheckShape(a, _)
            | NodeKind::LoadSlot(a, _)
            | NodeKind::LoadProtoSlot { recv: a, .. }
            | NodeKind::InlineUpvalue { closure: a, .. }
            | NodeKind::Float64Unary(_, a)
            | NodeKind::LoadArrayLength(a) => {
                vec![*a]
            }
            NodeKind::Int32Add(a, b)
            | NodeKind::Int32Sub(a, b)
            | NodeKind::Int32Mul(a, b)
            | NodeKind::Int32Compare(_, a, b)
            | NodeKind::Float64Add(a, b)
            | NodeKind::Float64Sub(a, b)
            | NodeKind::Float64Mul(a, b)
            | NodeKind::Float64Div(a, b)
            | NodeKind::Float64Compare(_, a, b)
            | NodeKind::StoreSlot(a, _, b)
            | NodeKind::LoadElement(a, b)
            | NodeKind::Int32BitOr(a, b)
            | NodeKind::Int32BitAnd(a, b)
            | NodeKind::Int32BitXor(a, b)
            | NodeKind::Int32Shl(a, b)
            | NodeKind::Int32Shr(a, b)
            | NodeKind::Int32UshrToFloat64(a, b) => vec![*a, *b],
            NodeKind::StoreElement(a, b, c) => vec![*a, *b, *c],
            NodeKind::LoadSlotPoly(a, _) => vec![*a],
            NodeKind::StoreSlotPoly(a, _, b) => vec![*a, *b],
            NodeKind::Phi(ops) => ops.clone(),
            NodeKind::Param(_)
            | NodeKind::ConstInt32(_)
            | NodeKind::ConstF64(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::ConstNull
            | NodeKind::SelfClosure
            | NodeKind::LoadUpvalue(_)
            | NodeKind::NewArray
            | NodeKind::LoadThis
            | NodeKind::LoadHole => Vec::new(),
            NodeKind::Call { inputs, .. } => inputs.clone(),
            NodeKind::AllocObjectLiteral { inputs, .. } => inputs.clone(),
            NodeKind::CallMethod { recv, args, .. } => {
                let mut inputs = Vec::with_capacity(args.len() + 1);
                inputs.push(*recv);
                inputs.extend(args.iter().copied());
                inputs
            }
            NodeKind::CheckFunctionIdentity { callee, .. } => vec![*callee],
            NodeKind::CheckMethodIdentity { recv, .. } => vec![*recv],
            NodeKind::MethodIdentityMatches { recv, .. } => vec![*recv],
            NodeKind::ArrayPop { recv } => vec![*recv],
            NodeKind::ArrayPush { recv, value, .. } => vec![*recv, *value],
        }
    }

    /// Rewrite every operand equal to `old` to `new`. Used by trivial-phi
    /// elimination to redirect all uses of a removed phi to its single distinct
    /// input.
    pub fn replace_input(&mut self, old: NodeId, new: NodeId) {
        let fix = |x: &mut NodeId| {
            if *x == old {
                *x = new;
            }
        };
        match self {
            NodeKind::CheckInt32(a)
            | NodeKind::CheckNumber(a)
            | NodeKind::CheckBool(a)
            | NodeKind::Int32ToFloat64(a)
            | NodeKind::Float64ToInt32(a)
            | NodeKind::TaggedIsNull { value: a, .. }
            | NodeKind::CheckShape(a, _)
            | NodeKind::LoadSlot(a, _)
            | NodeKind::LoadProtoSlot { recv: a, .. }
            | NodeKind::InlineUpvalue { closure: a, .. }
            | NodeKind::Float64Unary(_, a)
            | NodeKind::LoadArrayLength(a) => fix(a),
            NodeKind::Int32Add(a, b)
            | NodeKind::Int32Sub(a, b)
            | NodeKind::Int32Mul(a, b)
            | NodeKind::Int32Compare(_, a, b)
            | NodeKind::Float64Add(a, b)
            | NodeKind::Float64Sub(a, b)
            | NodeKind::Float64Mul(a, b)
            | NodeKind::Float64Div(a, b)
            | NodeKind::Float64Compare(_, a, b)
            | NodeKind::StoreSlot(a, _, b)
            | NodeKind::LoadElement(a, b)
            | NodeKind::Int32BitOr(a, b)
            | NodeKind::Int32BitAnd(a, b)
            | NodeKind::Int32BitXor(a, b)
            | NodeKind::Int32Shl(a, b)
            | NodeKind::Int32Shr(a, b)
            | NodeKind::Int32UshrToFloat64(a, b) => {
                fix(a);
                fix(b);
            }
            NodeKind::StoreElement(a, b, c) => {
                fix(a);
                fix(b);
                fix(c);
            }
            NodeKind::LoadSlotPoly(a, _) => fix(a),
            NodeKind::StoreSlotPoly(a, _, b) => {
                fix(a);
                fix(b);
            }
            NodeKind::Phi(ops) => ops.iter_mut().for_each(fix),
            NodeKind::LoadThis
            | NodeKind::LoadHole
            | NodeKind::LoadUpvalue(_)
            | NodeKind::NewArray
            | NodeKind::Param(_)
            | NodeKind::ConstInt32(_)
            | NodeKind::ConstF64(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::ConstNull
            | NodeKind::SelfClosure => {}
            NodeKind::Call { inputs, .. } => inputs.iter_mut().for_each(fix),
            NodeKind::AllocObjectLiteral { inputs, .. } => inputs.iter_mut().for_each(fix),
            NodeKind::CallMethod { recv, args, .. } => {
                fix(recv);
                args.iter_mut().for_each(fix);
            }
            NodeKind::CheckFunctionIdentity { callee, .. } => fix(callee),
            NodeKind::CheckMethodIdentity { recv, .. } => fix(recv),
            NodeKind::MethodIdentityMatches { recv, .. } => fix(recv),
            NodeKind::ArrayPop { recv } => fix(recv),
            NodeKind::ArrayPush { recv, value, .. } => {
                fix(recv);
                fix(value);
            }
        }
    }

    /// The representation a node of this kind produces.
    #[must_use]
    pub fn repr(&self) -> Repr {
        match self {
            NodeKind::ConstInt32(_)
            | NodeKind::CheckInt32(_)
            | NodeKind::Float64ToInt32(_)
            | NodeKind::LoadArrayLength(_)
            | NodeKind::Int32Add(_, _)
            | NodeKind::Int32Sub(_, _)
            | NodeKind::Int32Mul(_, _)
            | NodeKind::Int32BitOr(_, _)
            | NodeKind::Int32BitAnd(_, _)
            | NodeKind::Int32BitXor(_, _)
            | NodeKind::Int32Shl(_, _)
            | NodeKind::Int32Shr(_, _)
            | NodeKind::ArrayPush { .. } => Repr::Int32,
            NodeKind::ConstF64(_)
            | NodeKind::CheckNumber(_)
            | NodeKind::Int32ToFloat64(_)
            | NodeKind::Float64Add(_, _)
            | NodeKind::Float64Sub(_, _)
            | NodeKind::Float64Mul(_, _)
            | NodeKind::Float64Div(_, _)
            | NodeKind::Float64Unary(_, _)
            | NodeKind::Int32UshrToFloat64(_, _) => Repr::Float64,
            NodeKind::Int32Compare(_, _, _)
            | NodeKind::Float64Compare(_, _, _)
            | NodeKind::CheckBool(_)
            | NodeKind::MethodIdentityMatches { .. }
            | NodeKind::TaggedIsNull { .. } => Repr::Bool,
            // Register-carried values are tagged at block boundaries; a phi
            // therefore lives in tagged form (lowering boxes typed inputs).
            NodeKind::Param(_)
            | NodeKind::ConstBool(_)
            | NodeKind::ConstUndefined
            | NodeKind::ConstNull
            | NodeKind::SelfClosure
            | NodeKind::Phi(_)
            | NodeKind::LoadUpvalue(_)
            | NodeKind::NewArray
            | NodeKind::Call { .. }
            | NodeKind::AllocObjectLiteral { .. }
            | NodeKind::CallMethod { .. }
            | NodeKind::CheckFunctionIdentity { .. }
            | NodeKind::CheckMethodIdentity { .. }
            | NodeKind::CheckShape(_, _)
            | NodeKind::LoadSlot(_, _)
            | NodeKind::LoadProtoSlot { .. }
            | NodeKind::StoreSlot(_, _, _)
            | NodeKind::LoadSlotPoly(_, _)
            | NodeKind::StoreSlotPoly(_, _, _)
            | NodeKind::LoadElement(_, _)
            | NodeKind::StoreElement(_, _, _)
            | NodeKind::InlineUpvalue { .. }
            | NodeKind::ArrayPop { .. }
            | NodeKind::LoadThis
            | NodeKind::LoadHole => Repr::Tagged,
        }
    }
}

/// One SSA node: its operation, cached representation, and owning block.
#[derive(Clone, Debug)]
pub struct Node {
    /// The operation and its operand node ids.
    pub kind: NodeKind,
    /// Representation this node's value is produced in (cached from
    /// [`NodeKind::repr`]).
    pub repr: Repr,
    /// Block this node belongs to.
    pub block: BlockId,
    /// Byte-PC of the bytecode instruction this node serves. A node that can
    /// deoptimize (a `Check*` guard, an overflowing `Int32*`) stamps this so the
    /// interpreter resumes at the exact instruction. Synthetic entry defs use
    /// `0`.
    pub byte_pc: u32,
    /// Bytecode register this node's value is written through to, for deopt
    /// coherence: a freshly computed value that is the result of a
    /// register-writing instruction is boxed and stored to this frame slot so a
    /// later bail sees a current frame. `None` for temps (e.g. `Check*`) and
    /// values already resident in their frame slot (`Param`).
    pub frame_dst: Option<u16>,
}

/// Per-block control transfer.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Return the value of `NodeId` from the function.
    Return(NodeId),
    /// Unconditional branch to a block.
    Jump(BlockId),
    /// Two-way branch on a boolean predicate node. `on_true` is taken when the
    /// predicate is true.
    Branch {
        /// Boolean predicate node.
        cond: NodeId,
        /// Target when the predicate is true.
        on_true: BlockId,
        /// Target when the predicate is false.
        on_false: BlockId,
    },
    /// Leave compiled code: an instruction outside the optimizing subset was
    /// reached, restore the live interpreter registers and resume the interpreter
    /// at this byte-PC. Lets a function with a hot compilable loop and an
    /// un-compilable prologue / epilogue still compile and OSR the loop. No
    /// successors. A function containing one is entered ONLY through an OSR loop
    /// header — its function-entry runs the interpreter (see emit).
    Deopt(u32),
}

/// A basic block: a maximal straight-line instruction range plus its
/// predecessors, phis, body nodes, and terminator.
#[derive(Clone, Debug)]
pub struct Block {
    /// Byte-PC of the block's first bytecode instruction (its label).
    pub start_pc: u32,
    /// Predecessor blocks, in a fixed order phi operands are aligned to.
    pub preds: Vec<BlockId>,
    /// Phi node ids defined at the head of this block.
    pub phis: Vec<NodeId>,
    /// Straight-line body node ids in evaluation order (phis excluded).
    pub body: Vec<NodeId>,
    /// Control transfer leaving the block; `None` only mid-construction.
    pub term: Option<Terminator>,
    /// `true` once every predecessor edge is known and filled, so phi operands
    /// can be finalized (Braun et al. sealing).
    pub sealed: bool,
    /// `true` once the block's instructions have all been translated.
    pub filled: bool,
}

impl Block {
    pub(super) fn new(start_pc: u32) -> Self {
        Self {
            start_pc,
            preds: Vec::new(),
            phis: Vec::new(),
            body: Vec::new(),
            term: None,
            sealed: false,
            filled: false,
        }
    }
}

/// A typed SSA control-flow graph for one function.
#[derive(Clone, Debug)]
pub struct Graph {
    /// Node arena, indexed by [`NodeId`].
    pub nodes: Vec<Node>,
    /// Block arena, indexed by [`BlockId`].
    pub blocks: Vec<Block>,
    /// Entry block id (always `0`).
    pub entry: BlockId,
    /// Source function parameter count.
    pub param_count: u16,
    /// Source register-window length.
    pub register_count: u16,
    /// The bytecode register each `Phi` node merges, recorded at construction.
    /// Used by deopt frame-state reconstruction to know which register a header
    /// phi defines on block entry. Entries for trivially-eliminated phis become
    /// stale but are never read (only live `Block::phis` are consulted).
    pub phi_reg: rustc_hash::FxHashMap<NodeId, u16>,
    /// Per-block ordered log of bytecode-register definitions performed while
    /// translating the block's instructions: `(byte_pc, register, value)` in
    /// execution order. Unlike [`Node::frame_dst`] (which records only a value's
    /// *primary* destination), this captures *every* register rebind — including
    /// `LoadLocal` / `StoreLocal` aliasing that binds a register to an existing
    /// SSA value without producing a node. Deopt frame-state reconstruction
    /// replays it to know precisely which SSA value each interpreter register
    /// holds at a guard, which `frame_dst` alone cannot express for aliased
    /// registers.
    pub reg_writes: rustc_hash::FxHashMap<BlockId, Vec<(u32, u16, NodeId)>>,
}

impl Graph {
    /// Construct an empty graph with a single (entry) block at byte-PC 0.
    pub(super) fn new(param_count: u16, register_count: u16, entry: BlockId) -> Self {
        Self {
            nodes: Vec::new(),
            blocks: vec![Block::new(0)],
            entry,
            param_count,
            register_count,
            phi_reg: rustc_hash::FxHashMap::default(),
            reg_writes: rustc_hash::FxHashMap::default(),
        }
    }

    /// Append a node in `block` originating at `byte_pc`, returning its id. The
    /// repr is derived from the kind; `frame_dst` starts `None` (set by
    /// [`Self::set_frame_dst`] for register-writing instructions).
    pub(super) fn add_node(&mut self, kind: NodeKind, block: BlockId, byte_pc: u32) -> NodeId {
        let repr = kind.repr();
        let id = self.nodes.len() as NodeId;
        self.nodes.push(Node {
            kind,
            repr,
            block,
            byte_pc,
            frame_dst: None,
        });
        id
    }

    /// Mark `node` as written through to bytecode register `reg`.
    pub(super) fn set_frame_dst(&mut self, node: NodeId, reg: u16) {
        self.nodes[node as usize].frame_dst = Some(reg);
    }

    /// Borrow a node by id.
    #[must_use]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id as usize]
    }

    /// Borrow a block by id.
    #[must_use]
    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id as usize]
    }
}
