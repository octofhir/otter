//! Backend-neutral template operations over typed baseline lowering.
//!
//! # Contents
//! - [`TemplateOp`] — the complete machine-independent operation set every
//!   template backend consumes.
//! - [`TemplateInstr`] — one operation bound to its canonical instruction PC.
//! - [`TemplatePlan`] — the validated linear operation stream for one
//!   function.
//!
//! # Invariants
//! - Built strictly on top of the shared typed lowering pass: operand decoding,
//!   duplicate-PC detection, and branch-target verification happen exactly once
//!   before any backend opens an assembler.
//! - Structured exception opcodes carry pre-resolved canonical handlers into
//!   the shared VM completion transition; other unsupported opcodes become
//!   canonical-PC pre-effect side exits and make the body loop-OSR-only.
//! - Branch targets are canonical instruction indices already proven to name
//!   instruction boundaries; back edges are classified here so every backend
//!   places its cooperative poll identically.
//! - Immediate operands carry final boxed `Value` bit patterns; backends
//!   materialize them without consulting the constant pool.
//!
//! # See also
//! - [`super::arm64`] — the first machine-code consumer of these operations.

use otter_bytecode::Op;
use otter_vm::{JitCompileSnapshot, SafepointId, SafepointRecord, Value};
use std::fmt::Write as _;

use crate::entry::{BaselinePlan, MAX_METHOD_ARGS, Unsupported, pack_method_arg_regs, value_tag};

/// Numeric binary operators lowered to the shared int32/double template plus
/// the common VM numeric completion. `+` has its own operation
/// ([`TemplateOp::AddGeneric`]) because its non-numeric semantics are additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithKind {
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
}

/// Comparison operators lowered to the shared int32/double template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareKind {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

/// Int32 bitwise/shift operators sharing the `ToInt32` fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitwiseKind {
    Or,
    And,
    Xor,
    Shl,
    Shr,
}

/// One machine-independent template operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateOp {
    /// Store the boxed `Value` bit pattern into frame register `dst`.
    LoadImmediate { dst: u16, bits: u64 },
    /// Copy frame register `src` into frame register `dst`.
    Move { dst: u16, src: u16 },
    /// Unconditional branch to the canonical instruction PC `target`.
    Jump { target: u32, back_edge: bool },
    /// Branch to `target` when `ToBoolean(r<condition>)` matches
    /// `when_truthy`; fall through otherwise.
    Branch {
        condition: u16,
        target: u32,
        when_truthy: bool,
        back_edge: bool,
    },
    /// Branch to `target` when `r<condition>` is the `null` or `undefined`
    /// immediate; fall through for every other value without `ToBoolean`.
    BranchNullish {
        condition: u16,
        target: u32,
        back_edge: bool,
    },
    /// `r<dst> = ToBoolean(r<src>)`, inverted when `negate` is set.
    Truthiness { dst: u16, src: u16, negate: bool },
    /// `r<dst> = r<lhs> <op> r<rhs>` over tagged numbers: int32 fast path with
    /// overflow promotion to double, full double path, exact side exit on a
    /// non-number operand.
    BinaryArith {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: ArithKind,
    },
    /// `r<dst> = r<lhs> <cmp> r<rhs>` producing a boolean. Strict (in)equality
    /// additionally decides non-number immediates by raw identity; heap cells
    /// side-exit to the interpreter, which owns content equality.
    Compare {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: CompareKind,
    },
    /// Abstract (in)equality over numbers and the null/undefined equivalence
    /// class; every coercive case takes an exact side exit.
    LooseCompare {
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
    },
    /// Int32 bitwise/shift with the full finite-double `ToInt32` fast path.
    IntBitwise {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: BitwiseKind,
    },
    /// `r<dst> = r<lhs> >>> r<rhs>` with the full finite-double `ToUint32`
    /// fast path; the result boxes as a double (uint32-valued Number).
    UnsignedShiftRight { dst: u16, lhs: u16, rhs: u16 },
    /// `r<dst> = ToNumeric(r<src>) + delta` (update expressions).
    Increment { dst: u16, src: u16, delta: i32 },
    /// `r<dst> = -ToNumeric(r<src>)` including the `-0` and `-i32::MIN`
    /// double promotions.
    Negate { dst: u16, src: u16 },
    /// `r<dst> = ~ToNumeric(r<src>)`, with Number fast paths and BigInt
    /// completion through the shared numeric transition.
    BitwiseNot { dst: u16, src: u16 },
    /// `r<dst> = ToNumeric(r<src>)` — identity on numbers; coercive cases
    /// complete through the shared reentrant unary transition.
    ToNumeric { dst: u16, src: u16 },
    /// `r<dst> = ToPrimitive(r<src>, hint)` — identity on primitives;
    /// observable coercion hooks complete through the reentrant unary
    /// transition using the compiler-emitted hint token.
    ToPrimitive { dst: u16, src: u16, hint: u32 },
    /// `r<dst> = r<lhs> + r<rhs>` with the full `+` semantics: inline numeric
    /// paths, an allocating string-concat runtime call rooted through the
    /// published frame at `concat_safepoint`, and the interpreter-completing
    /// delegate for every remaining coercive case.
    AddGeneric {
        dst: u16,
        lhs: u16,
        rhs: u16,
        concat_safepoint: SafepointId,
    },
    /// `r<dst>` = this-binding read from the entry context; a derived-ctor
    /// hole takes an exact side exit.
    LoadThis { dst: u16 },
    /// `r<dst>` = the running function's SELF closure bits from the entry
    /// context (named-function self binding).
    LoadSelfClosure { dst: u16 },
    /// `r<dst>` = materialized function object for `constants[constant]`.
    MakeFunction { dst: u16, constant: u32 },
    /// `r<dst>` = closure over `function` capturing `parents` upvalues.
    MakeClosure {
        dst: u16,
        function: u32,
        parents: TemplateTail,
    },
    /// `r<dst> = constants[constant]` (string constant).
    LoadString { dst: u16, constant: u32 },
    /// Materialize a regex literal from the constant pool.
    LoadRegExp { dst: u16, constant: u32 },
    /// `r<dst> = global[name]` or throw.
    LoadGlobal { dst: u16, name: u32 },
    /// `r<dst>` = builtin error constructor for `constant`.
    LoadBuiltinError { dst: u16, constant: u32 },
    /// `r<dst> = {}`.
    NewObject { dst: u16 },
    /// `r<dst> = [elements…]` from the plan-owned register tail.
    NewArray { dst: u16, elements: TemplateTail },
    /// `r<dst> = Math.<method>(arguments…)`.
    MathCall {
        dst: u16,
        method: u32,
        arguments: TemplateTail,
    },
    /// Refresh the captured binding cell at `index` (per-iteration bindings).
    FreshUpvalue { index: i32 },
    /// `object[key] = value` as a data property definition.
    DefineDataProperty { object: u16, key: u16, value: u16 },
    /// `DefineOwnProperty(target, key, descriptor)`.
    DefineOwnProperty {
        target: u16,
        key: u16,
        descriptor: u16,
    },
    /// `r<dst> = r<receiver>[r<index>]`.
    LoadElement { dst: u16, receiver: u16, index: u16 },
    /// `r<receiver>[r<index>] = r<value>`.
    StoreElement {
        receiver: u16,
        index: u16,
        value: u16,
    },
    /// `r<dst> = upvalue[index]` (captured binding; TDZ raises in the VM).
    LoadUpvalue { dst: u16, index: i32 },
    /// `upvalue[index] = r<src>` (barriered store in the VM).
    StoreUpvalue { src: u16, index: i32 },
    /// `upvalue[index] = r<src>` with the TDZ read guard.
    StoreUpvalueChecked { src: u16, index: i32 },
    /// `r<dst> = r<object>.name` through the inline WhiskerIC probe. A miss
    /// completes the VM's full `[[Get]]` semantics in the window transition;
    /// it never exact-side-exits after invoking a getter or proxy trap.
    LoadProperty {
        dst: u16,
        object: u16,
        name: u32,
        site: u64,
        array_length: bool,
    },
    /// `r<object>.name = r<value>` through the inline WhiskerIC probe. A miss
    /// completes the VM's full `[[Set]]` semantics in the window transition;
    /// it never side-exits after invoking user code or committing a store.
    StoreProperty {
        object: u16,
        name: u32,
        value: u16,
        site: u64,
    },
    /// `r<dst> = r<callee>(args…)` through the direct-call prepare
    /// transition; ineligible callees take an exact side exit to normal
    /// dispatch. Argument register indices are packed one per 16-bit lane.
    Call {
        dst: u16,
        callee: u16,
        argc: u16,
        packed_args: u64,
        /// Serialized byte PC used to resolve the immutable call-site link.
        byte_pc: u32,
    },
    /// `r<dst> = new r<callee>(args…)` (`Op::New`) through the generic
    /// in-place construct transition; a non-constructor callee takes an exact
    /// side exit so the interpreter owns the thrown error. Argument register
    /// indices are packed one per 16-bit lane.
    Construct {
        dst: u16,
        callee: u16,
        argc: u16,
        packed_args: u64,
    },
    /// `r<dst> = r<receiver>.name(args…)` through the guarded collection
    /// fast paths, the collection-method IC, and the direct-method prepare
    /// transition. `byte_pc` keys the snapshot's per-site collection-method
    /// metadata; `arg0`/`arg1` are the first argument registers for the
    /// guarded typed-entry calls.
    MethodCall {
        dst: u16,
        receiver: u16,
        name: u32,
        site: u64,
        argc: u16,
        packed_args: u64,
        byte_pc: u32,
        arg0: Option<u16>,
        arg1: Option<u16>,
    },
    /// `r<dst> = r<callee>.bind(r<bound_this>, args…)` (`Op::BindFunction`)
    /// through the shared reentrant bind transition. Accessor `name`/`length`
    /// getters and bound-function allocation complete in the VM; a
    /// non-callable target reports a thrown `TypeError`. Argument register
    /// indices are packed one per 16-bit lane.
    BindFunction {
        dst: u16,
        callee: u16,
        bound_this: u16,
        argc: u16,
        packed_args: u64,
    },
    /// Install one pre-resolved structured-exception handler.
    EnterTry {
        catch_pc: Option<u32>,
        finally_pc: Option<u32>,
        exception_register: u16,
    },
    /// Leave the innermost handler, parking normal-finally completion when
    /// required.
    LeaveTry,
    /// Throw `r<src>` through the canonical VM unwind implementation.
    Throw { src: u16 },
    /// Materialize and throw the interpreter's TDZ `ReferenceError` for the
    /// encoded local index through the canonical VM unwind implementation.
    TdzError { local_index: u32 },
    /// Resume the completion parked by the active finally body.
    EndFinally,
    /// Abandon `count` parked finally completions.
    PopParkedFinally { count: u32 },
    /// Run finally bodies down to `floor`, then jump to `target`.
    JumpViaFinally { target: u32, floor: u32 },
    /// Advance an iterator through the VM's complete iterator transition.
    /// User `next`, generator resume, iterator helpers, and result-record
    /// accessors all complete in place through one reentrant runtime stub.
    IteratorNext {
        value_dst: u16,
        done_dst: u16,
        iterator: u16,
    },
    /// Complete `IteratorClose` through the VM's full `return`/generator/
    /// helper semantics.
    IteratorClose { iterator: u16 },
    /// Register an iterator for abrupt close in the current frame.
    IteratorCloseStart { iterator: u16 },
    /// Remove an iterator from the current frame's abrupt-close registry.
    IteratorCloseEnd { iterator: u16 },
    /// Obtain an iterator through the VM's full observable `@@iterator`
    /// transition: built-in iterables, user `[Symbol.iterator]()` methods
    /// (called synchronously through the shared reentrant path), and
    /// GetIteratorDirect `next` caching all complete in place.
    GetIterator { dst: u16, src: u16 },
    /// Obtain an async iterator (including async-from-sync fallback) through
    /// the VM's full observable `@@asyncIterator` transition.
    GetAsyncIterator { dst: u16, src: u16 },
    /// Complete one global-variable access (`LoadGlobalThis`,
    /// `LoadGlobalOrUndefined`, `StoreGlobalBinding`, `StoreGlobalChecked`)
    /// through the shared reentrant global environment-record transition.
    /// Accessor globals fire their getters/setters in the VM. `arg0`/`arg1`/
    /// `arg2` name the destination/value register, constant name index, and the
    /// opcode-specific strictness flag or `exists` register.
    GlobalOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one object property-protocol query (`Instanceof`,
    /// `HasProperty`, `GetPrototype`, `SetPrototype`) through the shared
    /// reentrant transition. Proxy `has`/`getPrototypeOf`/`setPrototypeOf` and
    /// `@@hasInstance` traps fire in the VM. `arg0`/`arg1`/`arg2` name the
    /// operand registers per opcode.
    ObjectProtocolOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one `delete` (`DeleteProperty`, `DeleteElement`,
    /// `DeleteDynamic`) through the shared reentrant delete transition. Proxy
    /// `deleteProperty` traps fire in the VM. `arg0`/`arg1`/`arg2` name the
    /// destination register, object register or name index, and name index or
    /// key register per opcode.
    DeleteOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one scalar value-query/coercion opcode (`ToObject`,
    /// `ToPropertyKey`, `TypeOf`, `LoadNewTarget`, `SameValue`, `IsArray`,
    /// `ArrayLength`, `LoadLength`) through guarded native fast paths where
    /// exact and the shared reentrant scalar fallback otherwise.
    /// `arg0`/`arg1`/`arg2` name the destination and source (or left/right)
    /// registers per opcode.
    ScalarOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one `super` property access (`LoadSuperProperty`,
    /// `LoadSuperElement`, `SetSuperProperty`, `SetSuperElement`) through the
    /// shared reentrant super transition. Home-prototype accessor
    /// getters/setters fire in the VM. `arg0`/`arg1`/`arg2` name the
    /// destination/home/name-or-key (or home/name-or-key/value) operands.
    SuperOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one private-member opcode (`PrivateGet`, `PrivateSet`,
    /// `PrivateBrandCheck`) through the shared reentrant private transition.
    /// Private accessor getters/setters fire in the VM. `arg0`/`arg1`/`arg2`
    /// name the operand registers per opcode.
    PrivateOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one static value-load opcode (`MathLoad`, `SymbolLoad`,
    /// `TemporalLoad`, `LoadBigInt`, `GetStringIndex`) through the shared
    /// reentrant value-load transition. `arg0`/`arg1`/`arg2` name the
    /// destination plus a constant name index or receiver/index registers.
    ValueLoadOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one allocating-construction opcode (`CollectRest`, `NewError`,
    /// `NewBuiltinError`, `ArrayPush`) through the shared reentrant construction
    /// transition. `arg0`/`arg1`/`arg2` name the destination plus source/value
    /// registers or a constant kind index.
    ConstructOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one structural object opcode (`ForInKeys`,
    /// `CopyDataProperties`) through the shared reentrant structural transition.
    /// `arg0`/`arg1` name the destination/target and source registers.
    StructuralOp { opcode: u8, arg0: u64, arg1: u64 },
    /// Complete one class-construction opcode (`BindThisValue`, `ClassCheck`,
    /// `SetFunctionName`) through the shared reentrant class transition.
    ClassOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete one variadic construction opcode (`ArrayConstruct`, `ArrayFrom`,
    /// `ArrayOf`, `QueueMicrotask`) through the shared reentrant variadic
    /// transition. `prefix` is the destination/callee register, `argc` the
    /// argument count, and `packed_args` the argument registers.
    VariadicOp {
        opcode: u8,
        prefix: u16,
        argc: u16,
        packed_args: u64,
    },
    /// Complete one static intrinsic-call opcode (`ArrayBufferCall`,
    /// `SharedArrayBufferCall`, `BigIntCall`, `DataViewCall`) through the shared
    /// reentrant static-call transition. `packed_head` is `dst | argc<<16`,
    /// `method` the method-id constant, and `packed_args` the argument
    /// registers.
    StaticCallOp {
        opcode: u8,
        packed_head: u64,
        method: u64,
        packed_args: u64,
    },
    /// Complete one dynamic control-family opcode (`LoadShadowedUpvalue`)
    /// through the shared reentrant VM transition.
    ControlOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete spread calls/constructions, explicit-receiver calls, and
    /// `CollectArguments` through the shared synchronous VM transition.
    /// `TailCall` is intentionally excluded — its interpreter completion
    /// discards the caller frame for true tail-call stack reuse, which the
    /// compiled call helper cannot reproduce, so it stays an exact side exit.
    SpreadCallOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete class creation, dynamic evaluation/function construction,
    /// template/private-name materialization, eval identity, and `ToNumber`
    /// through the shared VM transition.
    ClassValueOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// Complete synchronous module namespace/binding operations, star
    /// re-export, module-record marking, and `import.meta.resolve` through the
    /// shared VM transition. Promise-producing module operations remain exact
    /// side exits.
    ModuleOp {
        opcode: u8,
        arg0: u64,
        arg1: u64,
        arg2: u64,
    },
    /// No-op: advance to the next instruction with no effect (`Op::Nop`).
    NoOp,
    /// Return `r<src>` as the completion value.
    Return { src: u16 },
    /// Return `undefined` as the completion value.
    ReturnUndefined,
    /// Exact side exit at an opcode outside the compiled subset: the stamped
    /// PC names the uncommitted instruction and the interpreter resumes
    /// there. Code containing one is only sound to enter at a loop header
    /// via OSR (function entry would exit immediately).
    UnsupportedBail,
}

/// Range into a plan-owned homogeneous operand side buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateTail {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

/// One template operation at its canonical instruction PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateInstr {
    pub(crate) pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) op: TemplateOp,
}

/// Backend-neutral facts established before template emission starts.
///
/// The boxed operand buffers are address-stable: emitted variadic operations
/// bake pointers into them, and the finalized code object takes ownership of
/// the same allocations.
pub(crate) struct TemplatePlan {
    pub(crate) instructions: Vec<TemplateInstr>,
    pub(crate) register_count: u16,
    pub(crate) register_operands: Box<[u16]>,
    pub(crate) index_operands: Box<[u32]>,
    pub(crate) safepoint_records: Vec<SafepointRecord>,
    pub(crate) load_property_count: usize,
    pub(crate) store_property_count: usize,
    /// `true` when at least one opcode outside the subset was lowered to an
    /// exact side exit; such code serves loop OSR only.
    pub(crate) osr_only: bool,
}

/// Pack a call's argument registers inline (up to [`MAX_METHOD_ARGS`] u16
/// lanes) or spill a longer list into the plan's decoded register buffer,
/// returning the spill start index. `argc` discriminates the two encodings
/// end-to-end: the emitter rewrites a spilled index into the frozen buffer's
/// baked address, and runtime stubs decode by the same rule.
fn pack_or_spill_arg_regs(arguments: &[u16], register_operands: &mut Vec<u16>) -> u64 {
    if arguments.len() <= MAX_METHOD_ARGS {
        pack_method_arg_regs(arguments)
    } else {
        let start = register_operands.len() as u64;
        register_operands.extend_from_slice(arguments);
        start
    }
}

impl TemplatePlan {
    /// Deterministic text view of the already-built plan for artifact capture.
    pub(crate) fn render_artifact(&self) -> String {
        let mut out = String::from("; otter template plan v1\n");
        writeln!(
            out,
            "; registers={} register-operands={} index-operands={} safepoints={} osr-only={}",
            self.register_count,
            self.register_operands.len(),
            self.index_operands.len(),
            self.safepoint_records.len(),
            self.osr_only
        )
        .expect("writing to String cannot fail");
        writeln!(out, "; register-operands={:?}", self.register_operands)
            .expect("writing to String cannot fail");
        writeln!(out, "; index-operands={:?}", self.index_operands)
            .expect("writing to String cannot fail");
        for (index, instruction) in self.instructions.iter().enumerate() {
            writeln!(
                out,
                "{index:04} pc={:04} byte={:04} {:?}",
                instruction.pc, instruction.byte_pc, instruction.op
            )
            .expect("writing to String cannot fail");
        }
        out
    }

    pub(crate) fn register_tail(&self, tail: TemplateTail) -> &[u16] {
        &self.register_operands[tail.start..tail.start + tail.len]
    }

    /// Resolve a call's packed-argument word for emission: a spilled list
    /// (`argc > MAX_METHOD_ARGS`) becomes the baked address of its table in
    /// the frozen decoded-operand buffer and returns its stable logical range
    /// for relocation metadata; an inline pack passes through without a range.
    pub(crate) fn resolve_packed_args(
        &self,
        argc: u16,
        packed_args: u64,
    ) -> (u64, Option<TemplateTail>) {
        if usize::from(argc) > MAX_METHOD_ARGS {
            let tail = TemplateTail {
                start: packed_args as usize,
                len: usize::from(argc),
            };
            (self.register_tail(tail).as_ptr() as u64, Some(tail))
        } else {
            (packed_args, None)
        }
    }

    pub(crate) fn index_tail(&self, tail: TemplateTail) -> &[u32] {
        &self.index_operands[tail.start..tail.start + tail.len]
    }
}

impl TemplatePlan {
    pub(crate) fn build(view: &JitCompileSnapshot) -> Result<Self, Unsupported> {
        let lowering = BaselinePlan::build(view)?;
        let mut instructions = Vec::with_capacity(lowering.instructions.len());
        let mut register_operands: Vec<u16> = Vec::new();
        let mut index_operands: Vec<u32> = Vec::new();
        let mut osr_only = false;
        for (meta, lowered) in view.instructions.iter().zip(&lowering.instructions) {
            let pc = lowered.instruction_pc;
            let op = match lowered.op {
                Op::LoadInt32 => {
                    let operands = lowered.load_int32_operands()?;
                    TemplateOp::LoadImmediate {
                        dst: operands.dst,
                        bits: value_tag::box_int32(operands.value),
                    }
                }
                Op::LoadNumber => {
                    let dst = lowered.destination_operands()?.dst;
                    let value = meta
                        .load_number
                        .ok_or(Unsupported::OperandShape("load-number constant"))?;
                    TemplateOp::LoadImmediate {
                        dst,
                        bits: Value::number_f64(value).to_bits(),
                    }
                }
                Op::LoadUndefined => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_UNDEFINED,
                },
                Op::LoadNull => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_NULL,
                },
                Op::LoadTrue => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_TRUE,
                },
                Op::LoadFalse => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_FALSE,
                },
                Op::LoadHole => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_HOLE,
                },
                Op::LoadLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.value,
                        src: operands.local,
                    }
                }
                Op::StoreLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.local,
                        src: operands.value,
                    }
                }
                Op::Jump => {
                    let target = lowered.branch_operands()?.target;
                    TemplateOp::Jump {
                        target,
                        back_edge: target <= pc,
                    }
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let operands = lowered.conditional_branch_operands()?;
                    TemplateOp::Branch {
                        condition: operands.condition,
                        target: operands.target,
                        when_truthy: lowered.op == Op::JumpIfTrue,
                        back_edge: operands.target <= pc,
                    }
                }
                Op::JumpIfNullish => {
                    let operands = lowered.conditional_branch_operands()?;
                    TemplateOp::BranchNullish {
                        condition: operands.condition,
                        target: operands.target,
                        back_edge: operands.target <= pc,
                    }
                }
                Op::ToBoolean => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: false,
                    }
                }
                Op::LogicalNot => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: true,
                    }
                }
                Op::Add => {
                    let operands = lowered.binary_operands()?;
                    let concat_safepoint = lowering
                        .add_alloc_safepoints
                        .get(&lowered.byte_pc)
                        .copied()
                        .ok_or(Unsupported::OperandShape("Add without a safepoint"))?;
                    TemplateOp::AddGeneric {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        concat_safepoint,
                    }
                }
                Op::Sub | Op::Mul | Op::Div | Op::Rem | Op::Pow => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::BinaryArith {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::Sub => ArithKind::Sub,
                            Op::Mul => ArithKind::Mul,
                            Op::Div => ArithKind::Div,
                            Op::Rem => ArithKind::Rem,
                            _ => ArithKind::Pow,
                        },
                    }
                }
                Op::MakeFunction | Op::MakeClosure if meta.make_self => {
                    TemplateOp::LoadSelfClosure {
                        dst: lowered.destination_operands()?.dst,
                    }
                }
                Op::MakeFunction => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::MakeFunction {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::MakeClosure => {
                    let operands = lowered.make_closure_operands()?;
                    let slice = lowering.index_tail(operands.parents)?;
                    let start = index_operands.len();
                    index_operands.extend_from_slice(slice);
                    TemplateOp::MakeClosure {
                        dst: operands.dst,
                        function: operands.function,
                        parents: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::LoadThis => TemplateOp::LoadThis {
                    dst: lowered.destination_operands()?.dst,
                },
                Op::LoadString => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadString {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::LoadRegExp => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadRegExp {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::LoadGlobalOrThrow => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadGlobal {
                        dst: operands.dst,
                        name: operands.constant,
                    }
                }
                Op::LoadBuiltinError => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadBuiltinError {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::NewObject => TemplateOp::NewObject {
                    dst: lowered.destination_operands()?.dst,
                },
                Op::NewArray => {
                    let operands = lowered.new_array_operands()?;
                    let slice = lowering.register_tail(operands.elements)?;
                    let start = register_operands.len();
                    register_operands.extend_from_slice(slice);
                    TemplateOp::NewArray {
                        dst: operands.dst,
                        elements: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::MathCall => {
                    let operands = lowered.math_call_operands()?;
                    let slice = lowering.register_tail(operands.arguments)?;
                    let start = register_operands.len();
                    register_operands.extend_from_slice(slice);
                    TemplateOp::MathCall {
                        dst: operands.dst,
                        method: operands.method,
                        arguments: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::FreshUpvalue => TemplateOp::FreshUpvalue {
                    index: lowered.immediate_operands()?.value,
                },
                Op::DefineDataProperty => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::DefineDataProperty {
                        object: operands.first,
                        key: operands.second,
                        value: operands.third,
                    }
                }
                Op::DefineOwnProperty => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::DefineOwnProperty {
                        target: operands.first,
                        key: operands.second,
                        descriptor: operands.third,
                    }
                }
                Op::LoadElement => {
                    let operands = lowered.element_load_operands()?;
                    TemplateOp::LoadElement {
                        dst: operands.dst,
                        receiver: operands.receiver,
                        index: operands.index,
                    }
                }
                Op::StoreElement => {
                    let operands = lowered.element_store_operands()?;
                    TemplateOp::StoreElement {
                        receiver: operands.receiver,
                        index: operands.index,
                        value: operands.value,
                    }
                }
                Op::LoadUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::LoadUpvalue {
                        dst: operands.value,
                        index: operands.index,
                    }
                }
                Op::StoreUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::StoreUpvalue {
                        src: operands.value,
                        index: operands.index,
                    }
                }
                Op::StoreUpvalueChecked => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::StoreUpvalueChecked {
                        src: operands.value,
                        index: operands.index,
                    }
                }
                Op::LoadShadowedUpvalue => {
                    let operands = lowered.shadowed_upvalue_operands()?;
                    TemplateOp::ControlOp {
                        opcode: Op::LoadShadowedUpvalue as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.name),
                        arg2: operands.index as u64,
                    }
                }
                Op::LoadProperty => {
                    let operands = lowered.property_load_operands()?;
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::LoadProperty {
                        dst: operands.dst,
                        object: operands.object,
                        name: operands.name,
                        site,
                        array_length: meta.load_array_length,
                    }
                }
                Op::StoreProperty => {
                    let operands = lowered.property_store_operands()?;
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::StoreProperty {
                        object: operands.object,
                        name: operands.name,
                        value: operands.value,
                        site,
                    }
                }
                Op::Call => {
                    let operands = lowered.call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    TemplateOp::Call {
                        dst: operands.dst,
                        callee: operands.callee,
                        argc: arguments.len() as u16,
                        packed_args: pack_or_spill_arg_regs(arguments, &mut register_operands),
                        byte_pc: meta.byte_pc,
                    }
                }
                Op::CallWithThis => {
                    let operands = lowered.call_with_this_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        osr_only = true;
                        instructions.push(TemplateInstr {
                            pc,
                            byte_pc: lowered.byte_pc,
                            op: TemplateOp::UnsupportedBail,
                        });
                        continue;
                    }
                    TemplateOp::SpreadCallOp {
                        opcode: Op::CallWithThis as u8,
                        arg0: u64::from(operands.dst)
                            | (u64::from(operands.callee) << 16)
                            | (u64::from(operands.this_value) << 32)
                            | ((arguments.len() as u64) << 48),
                        arg1: pack_method_arg_regs(arguments),
                        arg2: 0,
                    }
                }
                Op::CallSpread => {
                    let operands = lowered.quad_operands()?;
                    TemplateOp::SpreadCallOp {
                        opcode: Op::CallSpread as u8,
                        arg0: u64::from(operands.first)
                            | (u64::from(operands.second) << 16)
                            | (u64::from(operands.third) << 32)
                            | (u64::from(operands.fourth) << 48),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::NewSpread | Op::SuperConstructSpread => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::SpreadCallOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::CollectArguments => TemplateOp::SpreadCallOp {
                    opcode: Op::CollectArguments as u8,
                    arg0: u64::from(lowered.destination_operands()?.dst),
                    arg1: 0,
                    arg2: 0,
                },
                Op::New => {
                    let operands = lowered.call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    TemplateOp::Construct {
                        dst: operands.dst,
                        callee: operands.callee,
                        argc: arguments.len() as u16,
                        packed_args: pack_or_spill_arg_regs(arguments, &mut register_operands),
                    }
                }
                Op::CallMethodValue => {
                    let operands = lowered.method_call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::MethodCall {
                        dst: operands.dst,
                        receiver: operands.receiver,
                        name: operands.name,
                        site,
                        argc: arguments.len() as u16,
                        packed_args: pack_or_spill_arg_regs(arguments, &mut register_operands),
                        byte_pc: lowered.byte_pc,
                        arg0: arguments.first().copied(),
                        arg1: arguments.get(1).copied(),
                    }
                }
                Op::BindFunction => {
                    let operands = lowered.bind_function_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        osr_only = true;
                        instructions.push(TemplateInstr {
                            pc,
                            byte_pc: lowered.byte_pc,
                            op: TemplateOp::UnsupportedBail,
                        });
                        continue;
                    }
                    TemplateOp::BindFunction {
                        dst: operands.dst,
                        callee: operands.callee,
                        bound_this: operands.bound_this,
                        argc: arguments.len() as u16,
                        packed_args: pack_method_arg_regs(arguments),
                    }
                }
                Op::EnterTry => {
                    let operands = lowered.exception_region_operands()?;
                    TemplateOp::EnterTry {
                        catch_pc: operands.catch_pc,
                        finally_pc: operands.finally_pc,
                        exception_register: operands.exception_register,
                    }
                }
                Op::LeaveTry => TemplateOp::LeaveTry,
                Op::Throw => TemplateOp::Throw {
                    src: lowered.source_operands()?.src,
                },
                Op::TdzError => TemplateOp::TdzError {
                    local_index: lowered.immediate_operands()?.value as u32,
                },
                Op::EndFinally => TemplateOp::EndFinally,
                Op::PopParkedFinally => {
                    let count = u32::try_from(lowered.immediate_operands()?.value)
                        .map_err(|_| Unsupported::OperandShape("PopParkedFinally count"))?;
                    TemplateOp::PopParkedFinally { count }
                }
                Op::JumpViaFinally => {
                    let operands = lowered.jump_via_finally_operands()?;
                    TemplateOp::JumpViaFinally {
                        target: operands.target,
                        floor: operands.floor,
                    }
                }
                Op::IteratorNext => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::IteratorNext {
                        value_dst: operands.first,
                        done_dst: operands.second,
                        iterator: operands.third,
                    }
                }
                Op::IteratorClose => TemplateOp::IteratorClose {
                    iterator: lowered.source_operands()?.src,
                },
                Op::IteratorCloseStart => TemplateOp::IteratorCloseStart {
                    iterator: lowered.source_operands()?.src,
                },
                Op::IteratorCloseEnd => TemplateOp::IteratorCloseEnd {
                    iterator: lowered.source_operands()?.src,
                },
                Op::GetIterator => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::GetIterator {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::GetAsyncIterator => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::GetAsyncIterator {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::LoadGlobalThis => {
                    let dst = lowered.destination_operands()?.dst;
                    TemplateOp::GlobalOp {
                        opcode: Op::LoadGlobalThis as u8,
                        arg0: u64::from(dst),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::LoadGlobalOrUndefined => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::GlobalOp {
                        opcode: Op::LoadGlobalOrUndefined as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::StoreGlobalBinding => {
                    let operands = lowered.global_store_operands()?;
                    TemplateOp::GlobalOp {
                        opcode: Op::StoreGlobalBinding as u8,
                        arg0: u64::from(operands.value),
                        arg1: u64::from(operands.name),
                        arg2: u64::from(operands.extra),
                    }
                }
                Op::StoreGlobalChecked
                | Op::DeclareGlobalVar
                | Op::DeclareGlobalLex
                | Op::ValidateGlobalDecl
                | Op::DefineGlobalVar
                | Op::DefineGlobalFunction
                | Op::InitGlobalLex
                | Op::GlobalBindingExists => {
                    let operands = lowered.global_store_operands()?;
                    TemplateOp::GlobalOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.value),
                        arg1: u64::from(operands.name),
                        arg2: u64::from(operands.extra),
                    }
                }
                Op::LoadDynamic | Op::StoreDynamic | Op::TypeofDynamic => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::GlobalOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::LoadSuperProperty => {
                    let operands = lowered.property_load_operands()?;
                    TemplateOp::SuperOp {
                        opcode: Op::LoadSuperProperty as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.object),
                        arg2: u64::from(operands.name),
                    }
                }
                Op::SetSuperProperty => {
                    let operands = lowered.global_store_operands()?;
                    TemplateOp::SuperOp {
                        opcode: Op::SetSuperProperty as u8,
                        arg0: u64::from(operands.value),
                        arg1: u64::from(operands.name),
                        arg2: u64::from(operands.extra),
                    }
                }
                Op::LoadSuperElement | Op::SetSuperElement => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::SuperOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::PrivateGet | Op::PrivateSet => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::PrivateOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::PrivateBrandCheck => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::PrivateOp {
                        opcode: Op::PrivateBrandCheck as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                        arg2: 0,
                    }
                }
                Op::MathLoad | Op::SymbolLoad | Op::TemporalLoad | Op::LoadBigInt => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::ValueLoadOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::GetStringIndex => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::ValueLoadOp {
                        opcode: Op::GetStringIndex as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::CollectRest => {
                    let dst = lowered.destination_operands()?.dst;
                    TemplateOp::ConstructOp {
                        opcode: Op::CollectRest as u8,
                        arg0: u64::from(dst),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::NewError
                | Op::ArrayPush
                | Op::NewWeakRef
                | Op::NewFinalizationRegistry
                | Op::PromiseFulfilledOf => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ConstructOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                        arg2: 0,
                    }
                }
                Op::NewBuiltinError | Op::NewCollection => {
                    let operands = lowered.global_store_operands()?;
                    TemplateOp::ConstructOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.value),
                        arg1: u64::from(operands.name),
                        arg2: u64::from(operands.extra),
                    }
                }
                Op::ForInKeys | Op::CopyDataProperties => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::StructuralOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                    }
                }
                Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf | Op::QueueMicrotask => {
                    let operands = lowered.new_array_operands()?;
                    let arguments = lowering.register_tail(operands.elements)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        osr_only = true;
                        instructions.push(TemplateInstr {
                            pc,
                            byte_pc: lowered.byte_pc,
                            op: TemplateOp::UnsupportedBail,
                        });
                        continue;
                    }
                    TemplateOp::VariadicOp {
                        opcode: lowered.op as u8,
                        prefix: operands.dst,
                        argc: arguments.len() as u16,
                        packed_args: pack_method_arg_regs(arguments),
                    }
                }
                Op::ArrayBufferCall
                | Op::SharedArrayBufferCall
                | Op::BigIntCall
                | Op::DataViewCall => {
                    let operands = lowered.static_call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        osr_only = true;
                        instructions.push(TemplateInstr {
                            pc,
                            byte_pc: lowered.byte_pc,
                            op: TemplateOp::UnsupportedBail,
                        });
                        continue;
                    }
                    TemplateOp::StaticCallOp {
                        opcode: lowered.op as u8,
                        packed_head: u64::from(operands.dst) | ((arguments.len() as u64) << 16),
                        method: u64::from(operands.method),
                        packed_args: pack_method_arg_regs(arguments),
                    }
                }
                Op::BindThisValue | Op::ClassCheck | Op::SetFunctionName => {
                    let operands = lowered.global_store_operands()?;
                    TemplateOp::ClassOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.value),
                        arg1: u64::from(operands.name),
                        arg2: u64::from(operands.extra),
                    }
                }
                Op::MakeClass => {
                    let operands = lowered.make_class_operands()?;
                    TemplateOp::ClassValueOp {
                        opcode: Op::MakeClass as u8,
                        arg0: u64::from(operands.dst)
                            | (u64::from(operands.ctor) << 16)
                            | (u64::from(operands.prototype) << 32)
                            | (u64::from(operands.statics) << 48),
                        arg1: u64::from(operands.parent),
                        arg2: 0,
                    }
                }
                Op::NewFunction => {
                    let operands = lowered.new_array_operands()?;
                    let arguments = lowering.register_tail(operands.elements)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        osr_only = true;
                        instructions.push(TemplateInstr {
                            pc,
                            byte_pc: lowered.byte_pc,
                            op: TemplateOp::UnsupportedBail,
                        });
                        continue;
                    }
                    TemplateOp::ClassValueOp {
                        opcode: Op::NewFunction as u8,
                        arg0: u64::from(operands.dst) | ((arguments.len() as u64) << 16),
                        arg1: pack_method_arg_regs(arguments),
                        arg2: 0,
                    }
                }
                Op::GetTemplateObject | Op::NewPrivateName => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::ClassValueOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::Eval => {
                    let operands = lowered.eval_operands()?;
                    TemplateOp::ClassValueOp {
                        opcode: Op::Eval as u8,
                        arg0: u64::from(operands.dst) | (u64::from(operands.src) << 16),
                        arg1: operands.flags as u32 as u64,
                        arg2: 0,
                    }
                }
                Op::IsEvalIntrinsic | Op::ToNumber => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ClassValueOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst) | (u64::from(operands.src) << 16),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::ImportNamespace | Op::ImportNamespaceDeferred | Op::ModuleNamespaceObject => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::ModuleOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::LoadImportBinding => {
                    let operands = lowered.import_binding_operands()?;
                    TemplateOp::ModuleOp {
                        opcode: Op::LoadImportBinding as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.module_url),
                        arg2: u64::from(operands.binding_name),
                    }
                }
                Op::StarReexport | Op::ImportMetaResolve => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ModuleOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                        arg2: 0,
                    }
                }
                Op::MarkModuleEvaluated => {
                    let operands = lowered.constant_only_operands()?;
                    TemplateOp::ModuleOp {
                        opcode: Op::MarkModuleEvaluated as u8,
                        arg0: u64::from(operands.constant),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::Nop => TemplateOp::NoOp,
                Op::Instanceof | Op::HasProperty => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::ObjectProtocolOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::GetPrototype | Op::SetPrototype => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ObjectProtocolOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                        arg2: 0,
                    }
                }
                Op::DeleteProperty => {
                    let operands = lowered.property_load_operands()?;
                    TemplateOp::DeleteOp {
                        opcode: Op::DeleteProperty as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.object),
                        arg2: u64::from(operands.name),
                    }
                }
                Op::DeleteElement => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::DeleteOp {
                        opcode: Op::DeleteElement as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::DeleteDynamic => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::DeleteOp {
                        opcode: Op::DeleteDynamic as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.constant),
                        arg2: 0,
                    }
                }
                Op::ToObject
                | Op::ToPropertyKey
                | Op::TypeOf
                | Op::IsArray
                | Op::ArrayLength
                | Op::LoadLength => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ScalarOp {
                        opcode: lowered.op as u8,
                        arg0: u64::from(operands.dst),
                        arg1: u64::from(operands.src),
                        arg2: 0,
                    }
                }
                Op::LoadNewTarget => {
                    let dst = lowered.destination_operands()?.dst;
                    TemplateOp::ScalarOp {
                        opcode: Op::LoadNewTarget as u8,
                        arg0: u64::from(dst),
                        arg1: 0,
                        arg2: 0,
                    }
                }
                Op::SameValue => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::ScalarOp {
                        opcode: Op::SameValue as u8,
                        arg0: u64::from(operands.first),
                        arg1: u64::from(operands.second),
                        arg2: u64::from(operands.third),
                    }
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::Compare {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::LessThan => CompareKind::Lt,
                            Op::LessEq => CompareKind::Le,
                            Op::GreaterThan => CompareKind::Gt,
                            Op::GreaterEq => CompareKind::Ge,
                            Op::Equal => CompareKind::Eq,
                            _ => CompareKind::Ne,
                        },
                    }
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::LooseCompare {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        negate: lowered.op == Op::LooseNotEqual,
                    }
                }
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::IntBitwise {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::BitwiseOr => BitwiseKind::Or,
                            Op::BitwiseAnd => BitwiseKind::And,
                            Op::BitwiseXor => BitwiseKind::Xor,
                            Op::Shl => BitwiseKind::Shl,
                            _ => BitwiseKind::Shr,
                        },
                    }
                }
                Op::Ushr => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::UnsignedShiftRight {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                    }
                }
                Op::Increment => {
                    let operands = lowered.increment_operands()?;
                    TemplateOp::Increment {
                        dst: operands.dst,
                        src: operands.src,
                        delta: operands.delta,
                    }
                }
                Op::Neg => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Negate {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::BitwiseNot => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::BitwiseNot {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::ToNumeric => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ToNumeric {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::ToPrimitive => {
                    let operands = lowered.to_primitive_operands()?;
                    TemplateOp::ToPrimitive {
                        dst: operands.dst,
                        src: operands.src,
                        hint: operands.hint,
                    }
                }
                Op::Return | Op::ReturnValue => TemplateOp::Return {
                    src: lowered.source_operands()?.src,
                },
                Op::ReturnUndefined => TemplateOp::ReturnUndefined,
                // Opcode outside the subset: lower to an exact side exit at
                // this PC instead of failing the whole compile, so a hot,
                // fully-supported loop still tiers up via OSR. The entry path
                // skips such code (`osr_only`).
                _ => {
                    osr_only = true;
                    TemplateOp::UnsupportedBail
                }
            };
            instructions.push(TemplateInstr {
                pc,
                byte_pc: lowered.byte_pc,
                op,
            });
        }
        Ok(Self {
            instructions,
            register_count: view.code_block.register_count,
            register_operands: register_operands.into_boxed_slice(),
            index_operands: index_operands.into_boxed_slice(),
            load_property_count: lowering.load_property_count,
            store_property_count: lowering.store_property_count,
            safepoint_records: lowering.safepoint_records,
            osr_only,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Operand;
    use otter_vm::jit::JitTestInstruction;

    const STRIDE: u32 = 4;

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 1, 8, instructions)
    }

    #[test]
    fn plan_maps_the_supported_subset() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::LoadTrue, vec![Operand::Register(1)]),
            (
                Op::LogicalNot,
                vec![Operand::Register(2), Operand::Register(1)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(1), Operand::Register(2)],
            ),
            (
                Op::StoreLocal,
                vec![Operand::Register(0), Operand::Imm32(3)],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(plan.register_count, 8);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::LoadImmediate {
                dst: 0,
                bits: value_tag::box_int32(-7),
            }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::LoadImmediate {
                dst: 1,
                bits: value_tag::VALUE_TRUE,
            }
        );
        assert_eq!(
            plan.instructions[2].op,
            TemplateOp::Truthiness {
                dst: 2,
                src: 1,
                negate: true,
            }
        );
        assert_eq!(
            plan.instructions[3].op,
            TemplateOp::Branch {
                condition: 2,
                target: 5,
                when_truthy: false,
                back_edge: false,
            }
        );
        assert_eq!(plan.instructions[4].op, TemplateOp::Move { dst: 3, src: 0 });
        assert_eq!(plan.instructions[5].op, TemplateOp::Return { src: 0 });
    }

    #[test]
    fn plan_classifies_back_edges() {
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (Op::Jump, vec![Operand::Imm32(-2)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::Jump {
                target: 0,
                back_edge: true,
            }
        );
        assert_eq!(plan.instructions[2].op, TemplateOp::ReturnUndefined);
    }

    #[test]
    fn plan_maps_structured_exception_region_completion() {
        let v = view(&[
            (
                Op::EnterTry,
                vec![
                    Operand::Imm32(1),
                    Operand::Imm32(otter_vm::NO_HANDLER_OFFSET),
                    Operand::Register(3),
                ],
            ),
            (Op::LeaveTry, vec![]),
            (Op::Throw, vec![Operand::Register(1)]),
            (Op::EndFinally, vec![]),
            (Op::PopParkedFinally, vec![Operand::Imm32(1)]),
            (
                Op::JumpViaFinally,
                vec![Operand::Imm32(0), Operand::Imm32(0)],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("exception plan");
        assert!(!plan.osr_only);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::EnterTry {
                catch_pc: Some(2),
                finally_pc: None,
                exception_register: 3,
            }
        );
        assert_eq!(plan.instructions[1].op, TemplateOp::LeaveTry);
        assert_eq!(plan.instructions[2].op, TemplateOp::Throw { src: 1 });
        assert_eq!(plan.instructions[3].op, TemplateOp::EndFinally);
        assert_eq!(
            plan.instructions[4].op,
            TemplateOp::PopParkedFinally { count: 1 }
        );
        assert_eq!(
            plan.instructions[5].op,
            TemplateOp::JumpViaFinally {
                target: 6,
                floor: 0,
            }
        );
    }

    #[test]
    fn plan_maps_iterator_lifecycle_completion() {
        let v = view(&[
            (Op::IteratorCloseStart, vec![Operand::Register(1)]),
            (
                Op::IteratorNext,
                vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::Register(1),
                ],
            ),
            (Op::IteratorClose, vec![Operand::Register(1)]),
            (Op::IteratorCloseEnd, vec![Operand::Register(1)]),
            (
                Op::GetAsyncIterator,
                vec![Operand::Register(4), Operand::Register(1)],
            ),
            (
                Op::GetIterator,
                vec![Operand::Register(5), Operand::Register(1)],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("iterator plan");
        assert!(!plan.osr_only);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::IteratorCloseStart { iterator: 1 }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::IteratorNext {
                value_dst: 2,
                done_dst: 3,
                iterator: 1,
            }
        );
        assert_eq!(
            plan.instructions[2].op,
            TemplateOp::IteratorClose { iterator: 1 }
        );
        assert_eq!(
            plan.instructions[3].op,
            TemplateOp::IteratorCloseEnd { iterator: 1 }
        );
        assert_eq!(
            plan.instructions[4].op,
            TemplateOp::GetAsyncIterator { dst: 4, src: 1 }
        );
        assert_eq!(
            plan.instructions[5].op,
            TemplateOp::GetIterator { dst: 5, src: 1 }
        );
    }

    #[test]
    fn plan_maps_synchronous_module_completion() {
        let v = view(&[
            (
                Op::ImportNamespace,
                vec![Operand::Register(0), Operand::ConstIndex(10)],
            ),
            (
                Op::ImportNamespaceDeferred,
                vec![Operand::Register(1), Operand::ConstIndex(11)],
            ),
            (
                Op::ModuleNamespaceObject,
                vec![Operand::Register(2), Operand::ConstIndex(12)],
            ),
            (
                Op::LoadImportBinding,
                vec![
                    Operand::Register(3),
                    Operand::ConstIndex(13),
                    Operand::ConstIndex(14),
                ],
            ),
            (
                Op::StarReexport,
                vec![Operand::Register(4), Operand::Register(5)],
            ),
            (Op::MarkModuleEvaluated, vec![Operand::ConstIndex(15)]),
            (
                Op::ImportMetaResolve,
                vec![Operand::Register(6), Operand::Register(7)],
            ),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("module plan");
        assert!(!plan.osr_only);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::ModuleOp {
                opcode: Op::ImportNamespace as u8,
                arg0: 0,
                arg1: 10,
                arg2: 0,
            }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::ModuleOp {
                opcode: Op::ImportNamespaceDeferred as u8,
                arg0: 1,
                arg1: 11,
                arg2: 0,
            }
        );
        assert_eq!(
            plan.instructions[2].op,
            TemplateOp::ModuleOp {
                opcode: Op::ModuleNamespaceObject as u8,
                arg0: 2,
                arg1: 12,
                arg2: 0,
            }
        );
        assert_eq!(
            plan.instructions[3].op,
            TemplateOp::ModuleOp {
                opcode: Op::LoadImportBinding as u8,
                arg0: 3,
                arg1: 13,
                arg2: 14,
            }
        );
        assert_eq!(
            plan.instructions[4].op,
            TemplateOp::ModuleOp {
                opcode: Op::StarReexport as u8,
                arg0: 4,
                arg1: 5,
                arg2: 0,
            }
        );
        assert_eq!(
            plan.instructions[5].op,
            TemplateOp::ModuleOp {
                opcode: Op::MarkModuleEvaluated as u8,
                arg0: 15,
                arg1: 0,
                arg2: 0,
            }
        );
        assert_eq!(
            plan.instructions[6].op,
            TemplateOp::ModuleOp {
                opcode: Op::ImportMetaResolve as u8,
                arg0: 6,
                arg1: 7,
                arg2: 0,
            }
        );
    }

    #[test]
    fn plan_maps_calls_properties_and_transitions() {
        let v = view(&[
            (
                Op::Call,
                vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (
                Op::CallMethodValue,
                vec![
                    Operand::Register(4),
                    Operand::Register(1),
                    Operand::ConstIndex(7),
                    Operand::ConstIndex(1),
                    Operand::Register(2),
                ],
            ),
            (
                Op::LoadProperty,
                vec![
                    Operand::Register(5),
                    Operand::Register(1),
                    Operand::ConstIndex(3),
                ],
            ),
            (
                Op::StoreProperty,
                vec![
                    Operand::Register(1),
                    Operand::ConstIndex(3),
                    Operand::Register(5),
                    Operand::Register(6),
                ],
            ),
            (
                Op::NewArray,
                vec![
                    Operand::Register(7),
                    Operand::ConstIndex(2),
                    Operand::Register(2),
                    Operand::Register(3),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        match plan.instructions[0].op {
            TemplateOp::Call {
                dst,
                callee,
                argc,
                packed_args,
                byte_pc: _,
            } => {
                assert_eq!((dst, callee, argc), (0, 1, 2));
                assert_eq!(packed_args & 0xffff, 2);
                assert_eq!((packed_args >> 16) & 0xffff, 3);
            }
            other => panic!("expected Call, got {other:?}"),
        }
        match plan.instructions[1].op {
            TemplateOp::MethodCall {
                dst,
                receiver,
                name,
                argc,
                arg0,
                ..
            } => {
                assert_eq!((dst, receiver, name, argc), (4, 1, 7, 1));
                assert_eq!(arg0, Some(2));
            }
            other => panic!("expected MethodCall, got {other:?}"),
        }
        assert!(matches!(
            plan.instructions[2].op,
            TemplateOp::LoadProperty {
                dst: 5,
                object: 1,
                name: 3,
                ..
            }
        ));
        assert!(matches!(
            plan.instructions[3].op,
            TemplateOp::StoreProperty {
                object: 1,
                name: 3,
                value: 5,
                ..
            }
        ));
        match plan.instructions[4].op {
            TemplateOp::NewArray { dst, elements } => {
                assert_eq!(dst, 7);
                assert_eq!(plan.register_tail(elements), &[2, 3]);
            }
            other => panic!("expected NewArray, got {other:?}"),
        }
        assert_eq!(plan.load_property_count, 1);
        assert_eq!(plan.store_property_count, 1);
    }

    #[test]
    fn plan_assigns_a_concat_safepoint_to_every_add() {
        let v = view(&[
            (
                Op::Add,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (Op::ReturnValue, vec![Operand::Register(2)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        let TemplateOp::AddGeneric {
            concat_safepoint, ..
        } = plan.instructions[0].op
        else {
            panic!("expected AddGeneric");
        };
        assert!(
            plan.safepoint_records
                .iter()
                .any(|record| record.id == concat_safepoint)
        );
    }

    #[test]
    fn plan_compiles_pow_and_bitwise_not_without_osr_fallback() {
        let v = view(&[
            (
                Op::Pow,
                vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            ),
            (
                Op::BitwiseNot,
                vec![Operand::Register(3), Operand::Register(2)],
            ),
            (Op::ReturnValue, vec![Operand::Register(3)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::BinaryArith {
                dst: 2,
                lhs: 0,
                rhs: 1,
                kind: ArithKind::Pow,
            }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::BitwiseNot { dst: 3, src: 2 }
        );
        assert!(!plan.osr_only);
    }

    #[test]
    fn plan_rejects_non_boundary_branch_targets() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(8)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(
            TemplatePlan::build(&v).err(),
            Some(Unsupported::BranchTarget(9))
        );
    }
}
