//! Otter foundation bytecode: container, opcode set, encoding, and dumps.
//!
//! This crate is the single source of truth for the new engine's
//! bytecode shape. It is consumed by `otter-compiler` (writers) and
//! `otter-vm` (readers / executors). It does **not** execute anything.
//!
//! # Contents
//! - [`Op`] — canonical opcode enum (`Nop`, `LoadUndefined`, `Return`
//!   for the harness slice; extended slice-by-slice).
//! - [`FunctionCode`] / [`WordInstruction`] — authoritative compact execution
//!   wordcode and schema-driven operand access.
//! - [`Instruction`] — cold decoded wire/debug DTO.
//! - [`Function`] — one compiled function: registers, authoritative wordcode,
//!   spans, and constants index.
//! - [`BytecodeModule`] — top-level container the compiler emits and
//!   the VM consumes.
//! - [`disasm`] — text disassembler for CLI/debug output.
//! - [`dump`] — JSON dump for tooling and tests
//!   (`otterBytecodeDumpVersion: 1`).
//! - [`opcode_schema`] — declarative opcode identity, wire-format, conservative
//!   effects, and tier-policy metadata.
//!
//! # Invariants
//! - An instruction's index in [`Function::code`] is its logical PC; spans
//!   inside [`Function::spans`] are sorted by logical PC.
//! - Decoded [`Instruction`] values and byte PCs never enter the hot execution
//!   representation.
//! - Mnemonics are `SCREAMING_SNAKE_CASE` and match the strings the
//!   disassembler emits.
//! - Opcode byte assignments have one source in [`opcode_schema`]; encoding
//!   retains a generated compatibility view of the unchanged wire format.
//!
//! # See also
//! - [Frontend and compilation](../../../docs/book/src/engine/frontend.md)

pub mod disasm;
pub mod dump;
pub mod encoding;
pub mod method_id;
pub mod opcode_audit;
pub mod opcode_schema;
pub mod wordcode;

pub use wordcode::{
    FunctionCode, FunctionCodeBuilder, Instruction as WordInstruction,
    OperandView as WordOperandView,
};

use serde::{Deserialize, Serialize};

/// Sentinel offset value that means "this try block does not have
/// a catch (or finally) clause". Picked as `i32::MIN` so any real
/// PC delta the compiler emits stays clear of it. The dispatcher
/// translates the sentinel to `Option::None` when reading
/// [`Op::EnterTry`] operands; the compiler installs it for the
/// absent clause when emitting the instruction.
pub const NO_HANDLER_OFFSET: i32 = i32::MIN;

/// The canonical foundation opcode set.
///
/// The harness slice (task 07) provides only the three opcodes
/// required to compile and execute the smoke fixtures
/// (`empty-script.ts`, `literal-undefined.ts`). Slice tasks
/// `09`–`13` extend this enum.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// No operation. Used as a placeholder; cost: one dispatch tick.
    Nop,
    /// `r<dst> = undefined`.
    LoadUndefined,
    /// `r<dst> = <array hole sentinel>`.
    ///
    /// Emitted by the compiler for elision elements in array
    /// literals (`[1, , 3]`) so the resulting `Op::NewArray` /
    /// `Op::ArrayPush` operand carries the internal `Value::Hole`
    /// instead of an explicit `undefined`. User code never observes
    /// this register slot directly — every array read path maps
    /// `Value::Hole` back to `undefined` per ECMA-262 §10.4.2.
    LoadHole,
    /// Return from the current function with `r<src>` as the
    /// completion value.
    Return,
    /// `r<dst> = constants[k<idx>]` (string constant).
    LoadString,
    /// `r<dst> = constants[k<idx>]` (number constant).
    LoadNumber,
    /// `r<dst> = imm:i32` (small-integer immediate via
    /// `Operand::ConstIndex` — the constant pool holds the literal).
    LoadInt32,
    /// `r<dst> = constants[k<idx>]` (BigInt constant). The
    /// constant carries the literal's decimal digits without the
    /// trailing `n` suffix; the runtime parses them at load time
    /// into a `Value::BigInt`. Compile-time validation guarantees
    /// the digits are syntactically valid.
    LoadBigInt,
    /// `r<dst> = constants[k<idx>]` (RegExp constant). The constant
    /// carries the WTF-16 pattern body and the ASCII flag string
    /// (`"dgimsuvy"` subset). The runtime compiles the regex once at
    /// load time; subsequent loads of the same constant share the
    /// compiled engine via the constant pool slot.
    LoadRegExp,
    /// Enqueue a microtask: `queueMicrotask(callee, args...)`.
    /// Operands: `callee_reg, argc, args...`. There is no
    /// destination register — the global returns `undefined`
    /// synchronously and the caller writes that itself with
    /// [`Op::LoadUndefined`] when needed.
    ///
    /// The runtime stores `(callee, this=undefined, args)` on the
    /// per-interpreter microtask queue and drains it after the
    /// currently-running script completes (success or error).
    QueueMicrotask,
    /// `r<dst> = new Promise(executor)`. Operands: `dst,
    /// executor_reg, scratch_dst`.
    ///
    /// Builds a fresh pending promise, materialises native
    /// `resolve` / `reject` closures that capture the promise,
    /// writes the promise into `dst`, then invokes `executor` with
    /// `[resolve, reject]`. The executor's return value lands in
    /// `scratch_dst` (and is ignored). If the executor throws
    /// synchronously the throw propagates as a runtime error
    /// today; spec-faithful "reject the promise" treatment lands
    /// when try/catch around native calls is wired.
    PromiseNew,
    /// `r<dst> = Promise.<name>(args...)`. Operands:
    /// `dst, name_const, argc, args...`. Variadic, same shape as
    /// the other namespace-call shortcuts. Resolves `<name>` against
    /// the Promise statics dispatcher; unknown names surface as
    /// `UnknownIntrinsic`.
    PromiseCall,
    /// `r<dst> = true`.
    LoadTrue,
    /// `r<dst> = false`.
    LoadFalse,
    /// `r<dst> = r<src>.length` (string operand). Returns Number.
    LoadLength,
    /// `r<dst> = r<recv>[r<idx>]` for string operand. Out-of-range
    /// yields the empty string.
    GetStringIndex,
    /// Universal variadic method call. Operands:
    /// `dst, recv, name_const, argc, args...`.
    ///
    /// At runtime the dispatcher branches on the receiver kind:
    /// - Builtin prototype methods — dispatches through the matching
    ///   native method path.
    /// - `Object` — loads the property; raises `TypeMismatch` when
    ///   the property is missing or not callable; otherwise calls
    ///   the resolved function with `this` bound to the receiver.
    /// - `Function` / `Closure` / `BoundFunction` — dispatches the
    ///   `call`, `apply`, and `bind` shapes through the same path
    ///   so dynamic `f["call"](...)` keeps working.
    /// - Anything else — `TypeMismatch`.
    CallMethodValue,

    // Polymorphic binary operators. Operands: `dst, lhs, rhs`.
    // Handle Number+Number and String+String operand pairs;
    // mixed types raise `TypeMismatch` until later slices add
    // coercion.
    /// `r<dst> = r<lhs> + r<rhs>` (Number+Number or String+String).
    Add,
    /// `r<dst> = r<lhs> - r<rhs>` (Number+Number).
    Sub,
    /// `r<dst> = r<lhs> * r<rhs>` (Number+Number).
    Mul,
    /// `r<dst> = r<lhs> / r<rhs>` (Number+Number).
    Div,
    /// `r<dst> = r<lhs> % r<rhs>` (Number+Number).
    Rem,
    /// `r<dst> = -r<src>` (Number).
    Neg,
    /// `r<dst> = r<lhs> ** r<rhs>` (Number).
    Pow,
    /// `r<dst> = r<lhs> & r<rhs>` after `ToInt32` on both operands.
    BitwiseAnd,
    /// `r<dst> = r<lhs> | r<rhs>` after `ToInt32` on both operands.
    BitwiseOr,
    /// `r<dst> = r<lhs> ^ r<rhs>` after `ToInt32` on both operands.
    BitwiseXor,
    /// `r<dst> = ~r<src>` after `ToInt32`.
    BitwiseNot,
    /// `r<dst> = r<lhs> << (r<rhs> & 31)` after `ToInt32` /
    /// `ToUint32` per spec.
    Shl,
    /// `r<dst> = r<lhs> >> (r<rhs> & 31)` (arithmetic / sign-
    /// preserving shift).
    Shr,
    /// `r<dst> = r<lhs> >>> (r<rhs> & 31)` (logical / zero-fill
    /// shift). The result is `ToUint32`, so values that would
    /// overflow `i32` are returned as a `Double`.
    Ushr,
    /// `r<dst> = ToNumber(r<src>)` (foundation subset).
    ToNumber,
    /// `r<dst> = (r<lhs> === r<rhs>)`. Returns Boolean.
    Equal,
    /// `r<dst> = (r<lhs> !== r<rhs>)`. Returns Boolean.
    NotEqual,
    /// `r<dst> = (r<lhs> < r<rhs>)`. Number+Number or String+String.
    LessThan,
    /// `r<dst> = (r<lhs> <= r<rhs>)`.
    LessEq,
    /// `r<dst> = (r<lhs> > r<rhs>)`.
    GreaterThan,
    /// `r<dst> = (r<lhs> >= r<rhs>)`.
    GreaterEq,

    /// `r<dst> = null`.
    LoadNull,
    /// `r<dst> = !ToBoolean(r<src>)`.
    LogicalNot,
    /// `r<dst> = ToBoolean(r<src>)` — explicit coercion used by
    /// branch operands the compiler cannot statically prove are
    /// boolean.
    ToBoolean,
    /// Unconditional relative branch: `pc += imm32(rel)`.
    /// Operand: `Imm32(signed_offset)`. Offset is relative to the
    /// **next** instruction.
    Jump,
    /// Branch when `ToBoolean(r<cond>)` is true. Operands:
    /// `Imm32(signed_offset), Register(cond)`.
    JumpIfTrue,
    /// Branch when `ToBoolean(r<cond>)` is false.
    JumpIfFalse,
    /// Branch when `r<cond>` is `null` or `undefined`. Used for
    /// nullish coalescing `??`.
    JumpIfNullish,
    /// `r<dst> = locals[idx]`. Operands:
    /// `Register(dst), Imm32(local_index)`.
    LoadLocal,
    /// `locals[idx] = r<src>`. Operands:
    /// `Register(src), Imm32(local_index)`.
    StoreLocal,
    /// Throw a `ReferenceError` for a TDZ-violating local read.
    /// Operand: `Imm32(local_index)`. Used until full lexical
    /// environments arrive.
    TdzError,

    /// `r<dst> = function-value(constants[k<idx>])`. The constant
    /// is a [`Constant::FunctionId`] referencing
    /// [`BytecodeModule::functions`].
    MakeFunction,
    /// `r<dst> = closure(constants[k<idx>], upvalues...)`. Variadic.
    /// Operands: `dst, function_const, upvalue_count, src0, src1, ...`.
    /// Each `srcN` is `Imm32(parent_upvalue_idx)` — a non-negative
    /// index into the **enclosing** frame's `upvalues` array. The
    /// runtime clones each GC cell handle into the new closure's
    /// upvalue spine, so writes through one are visible through all.
    ///
    /// Captured locals always live in the declaring frame's own
    /// upvalue cells (see [`Function::own_upvalue_count`]); a fresh
    /// frame appends `own_upvalue_count` empty cells after the
    /// inherited parent ones, and the function body initialises them
    /// via [`Op::StoreUpvalue`]. Subsequent `MakeClosure` calls just
    /// pick those indices off the current frame's `upvalues`.
    MakeClosure,
    /// `r<dst> = upvalue<idx>` — read the captured cell at index
    /// `idx` in the current frame's upvalue table.
    /// Operands: `Register(dst), Imm32(upvalue_idx)`.
    LoadUpvalue,
    /// `upvalue<idx> = r<src>` — write the captured cell at index
    /// `idx` in the current frame's upvalue table.
    /// Operands: `Register(src), Imm32(upvalue_idx)`.
    StoreUpvalue,
    /// Like [`Op::StoreUpvalue`], but raises `ReferenceError` when the
    /// target cell still holds the Temporal Dead Zone hole. Emitted for
    /// an *assignment* (PutValue, §6.2.4.6) to a captured `let` / `const`
    /// binding — a write before the declaration's initializer ran. The
    /// binding-initialization stores keep using [`Op::StoreUpvalue`],
    /// which legitimately clears the hole.
    /// Operands: `Register(src), Imm32(upvalue_idx)`.
    StoreUpvalueChecked,
    /// Replace own-upvalue cell `idx` with a freshly allocated cell
    /// holding a hole (Temporal Dead Zone). Operands: `Imm32(idx)`.
    /// Closures created *before* this op keep the previous cell, so a
    /// `for (let x of …)` body materialises a distinct `x` per
    /// iteration (§14.7.5.6 CreatePerIterationEnvironment) and a head
    /// `let` name spends RHS evaluation in the TDZ (§14.7.5.12
    /// ForIn/OfHeadEvaluation). A subsequent [`Op::StoreUpvalue`]
    /// writes the iteration's value into the new cell; reading the hole
    /// through [`Op::LoadUpvalue`] throws a `ReferenceError`.
    FreshUpvalue,
    /// Variadic call. Operands: `dst, callee, argc, args...`. The
    /// callee must be a function value at this slice. The callee
    /// receives `this = undefined` (foundation default).
    Call,
    /// Proper-tail call (§15.10.3). Same operand layout as
    /// [`Op::Call`] (`dst, callee, argc, args...`) and the same
    /// `this = undefined` default. Emitted by the compiler only for a
    /// call in a strict-mode tail position that is not enclosed by a
    /// `try`/`finally`. The dispatcher **replaces** the current frame
    /// with the callee's frame instead of pushing a new one, so a
    /// self-recursive tail call runs in O(1) native stack. When the
    /// caller frame can't be discarded safely (constructor, async, or
    /// an active handler), the dispatcher falls back to ordinary
    /// [`Op::Call`] semantics using `dst`.
    TailCall,
    /// Variadic call with an explicit receiver. Operands:
    /// `dst, callee, this, argc, args...`. Used by
    /// `Function.prototype.call` / `apply` lowering.
    CallWithThis,
    /// `r<dst> = bound function`. Operands:
    /// `dst, callee, this, argc, args...`. Builds a
    /// `Value::BoundFunction` that, when invoked, forwards to
    /// `callee` with `this` and `args` prepended to call-site
    /// arguments. Used by `Function.prototype.bind` lowering.
    BindFunction,
    /// `r<dst> = current frame's `this` value`. Operand: `dst`.
    /// Module top level binds `this` to `undefined` (foundation
    /// strict default).
    LoadThis,
    /// `r<dst> = current constructor call's `new.target`, or
    /// `undefined` when the active frame was not entered through
    /// `[[Construct]]`. Operand: `dst`.
    LoadNewTarget,
    /// Throw `r<src>` as an exception. Operand: `Register(src)`.
    /// Walks the frame's handler stack; on miss, pops the frame and
    /// continues the search in the caller. An exception that
    /// reaches the bottom of the call stack surfaces through the
    /// public API as `OtterError::Runtime` with `code = "UNCAUGHT"`.
    Throw,
    /// Push a try-handler entry onto the current frame's handler
    /// stack. Operands:
    /// `Imm32(catch_offset), Imm32(finally_offset), Register(exc_dst)`.
    /// Offsets are signed PC deltas relative to the **next**
    /// instruction (matching `Op::Jump`'s convention). A negative
    /// or sentinel `i32::MIN` offset means "no such handler"; the
    /// dispatcher treats absent offsets accordingly. `exc_dst` is
    /// the register the catch clause expects the thrown value in;
    /// for try/finally without a catch, the compiler still picks a
    /// scratch register but the dispatcher leaves it untouched on
    /// the finally path.
    EnterTry,
    /// Pop the most recent try-handler entry pushed by
    /// [`Op::EnterTry`] in the current frame. No operands.
    LeaveTry,
    /// Re-throw any in-flight exception that was parked on the
    /// frame when a throw routed through a `finally` block. No
    /// operands. When no throw is parked the dispatcher falls
    /// through, so a `finally` running on the success path is a
    /// silent no-op.
    EndFinally,
    /// `r<dst> = new Error(r<msg>)`. Operands:
    /// `Register(dst), Register(msg)`. Materialises a foundation
    /// `Error` object with `name = "Error"` and `message = msg`,
    /// where `msg` is coerced to its display string when not
    /// already a `String`. Used by both `new Error(x)` and
    /// `Error(x)` lowering — the foundation makes no observable
    /// distinction yet (subclassing arrives with task 26).
    NewError,
    /// Park a generator after call-time parameter/body-instantiation
    /// prologue. No operands.
    GeneratorStart,

    /// `r<dst> = GetIterator(r<src>)`. Operands:
    /// `Register(dst), Register(src)`. The runtime produces an
    /// internal iterator value over the source: `Array` walks
    /// elements, `String` walks code units, and user objects route
    /// through `[@@iterator]`.
    GetIterator,
    /// `r<dst> = GetAsyncIterator(r<src>)`. Operands:
    /// `Register(dst), Register(src)`. The runtime first observes
    /// `[@@asyncIterator]`; when absent it falls back to a sync
    /// iterator value for async-from-sync delegation.
    GetAsyncIterator,
    /// Drive an iterator one step. Operands:
    /// `Register(value_dst), Register(done_dst), Register(iter)`.
    /// Writes the next value into `value_dst` and a `Boolean` into
    /// `done_dst`; once `done_dst` is `true` the value is
    /// `undefined` and further calls keep returning `done = true`.
    IteratorNext,
    /// Close an iterator. Operands: `Register(iter)`.
    IteratorClose,
    /// Register an iterator for abrupt close while the frame is parked.
    /// Operands: `Register(iter)`.
    IteratorCloseStart,
    /// Remove a registered iterator after destructuring completes.
    /// Operands: `Register(iter)`.
    IteratorCloseEnd,
    /// Append `r<value>` to the array in `r<arr>`. Operands:
    /// `Register(arr), Register(value)`. No result. Used by the
    /// spread lowering for array literals.
    ArrayPush,
    /// Variadic-by-array call. Operands:
    /// `Register(dst), Register(callee), Register(this),
    /// Register(args)`. The args register holds a `Value::Array`
    /// whose elements become the call arguments, in order. Used
    /// by spread in call expressions (`f(...arr)` and friends).
    CallSpread,
    /// Construct call (the `new` expression). Operands:
    /// `Register(dst), Register(callee), ConstIndex(argc),
    /// Register(arg0), …`. The runtime allocates a fresh object
    /// whose `[[Prototype]]` is `callee.prototype` (or `null` when
    /// the callee has no `prototype` own property), invokes the
    /// callee with `this` bound to the new object, and writes the
    /// result. If the callee returns an object, that object becomes
    /// the result; otherwise the freshly allocated object is used,
    /// matching the spec's `OrdinaryCreateFromConstructor` behavior
    /// stripped down for the foundation slice.
    New,
    /// Construct call with spread arguments. Operands:
    /// `Register(dst), Register(callee), Register(args)`. The args
    /// register holds a `Value::Array` whose elements become the
    /// constructor arguments, in order. Mirrors [`Self::CallSpread`]
    /// for `new C(...args)` style invocations.
    NewSpread,
    /// Derived-class `super(...args)` construct with spread
    /// arguments. Operands match [`Self::NewSpread`], but the VM
    /// forwards the current frame's `new.target` instead of using
    /// the superclass callee as `new.target`.
    SuperConstructSpread,
    /// Bind the derived-constructor `this` to the value produced by
    /// a `super(...)` call (§13.3.7.3 SuperCall, steps 7–9 —
    /// `BindThisValue`). Operand: `Register(src)`. Reads the super
    /// result from `src`, installs it as the frame's `this`, marks
    /// `this` initialized, and records it as the construct target so
    /// an implicit `return` yields the bound object. Throws a
    /// `ReferenceError` if `this` was already initialized (i.e.
    /// `super()` ran twice).
    BindThisValue,
    /// `super.name` read (§13.3.5 MakeSuperPropertyReference +
    /// GetValue). Operands: `Register(dst), Register(home),
    /// ConstIndex(name)`. Resolves against
    /// `Object.getPrototypeOf(home)` but invokes any accessor getter
    /// with the *current* frame's `this` as the receiver. Throws a
    /// ReferenceError if `this` is in the TDZ and a TypeError if the
    /// resolved super-base is `null`/`undefined`.
    LoadSuperProperty,
    /// `super[key]` read — computed-key form of
    /// [`Self::LoadSuperProperty`]. Operands: `Register(dst),
    /// Register(home), Register(key)`.
    LoadSuperElement,
    /// `super.name = value` write (§13.3.5 + §6.2.5.5 PutValue
    /// step 6.b). Operands: `Register(home), ConstIndex(name),
    /// Register(value)`. Resolves any accessor setter against
    /// `Object.getPrototypeOf(home)` and invokes it with the current
    /// `this` as receiver; otherwise writes an own data property onto
    /// `this`. TDZ / null-base errors match [`Self::LoadSuperProperty`].
    SetSuperProperty,
    /// `super[key] = value` write — computed-key form of
    /// [`Self::SetSuperProperty`]. Operands: `Register(home),
    /// Register(key), Register(value)`.
    SetSuperElement,
    /// `break` / `continue` that crosses one or more `finally` blocks
    /// (§14.15.3). Operands: `Imm32(offset), Imm32(floor)`. Runs the
    /// crossed `finally` blocks (popping the frame's try-handlers down
    /// to `floor`), then jumps to `pc + 1 + offset`. `offset` is
    /// patched like an ordinary branch target.
    JumpViaFinally,
    /// `r<dst> = bool` — whether the global environment currently has a
    /// binding named by the string constant (script lexicals or a
    /// global-object property, own or inherited). Snapshot taken when a
    /// strict unresolved-identifier assignment evaluates its LHS
    /// reference, BEFORE the RHS runs (§6.2.5.6 PutValue over an
    /// unresolvable reference). Operands: `Register(dst),
    /// ConstIndex(name)`.
    GlobalBindingExists,
    /// Strict global store gated on a pre-RHS existence snapshot: when
    /// `r<exists>` is false the store throws the unresolved-identifier
    /// ReferenceError even if the RHS created the property meanwhile.
    /// Operands: `Register(value), ConstIndex(name), Register(exists)`.
    StoreGlobalChecked,
    /// Discard the innermost `count` completions parked by enclosing
    /// `finally` blocks (§14.15.3 — a `break`/`continue` that exits a
    /// finally body abandons the completion that finally had parked).
    /// Operands: `Imm32(count)`.
    PopParkedFinally,
    /// `r<dst> = ClassConstructor { ctor, prototype, statics }`.
    /// Operands: `Register(dst), Register(ctor), Register(prototype),
    /// Register(statics)`. Used by class lowering to package the
    /// constructor callable, instance-side prototype object, and
    /// static-side object into a single first-class value.
    MakeClass,
    /// Read a constant or other read-only property off the
    /// `Math` namespace. Operands:
    /// `Register(dst), ConstIndex(name)`. Used by the compiler
    /// when it sees `Math.PI` / `Math.E` / `Math.<known>` outside
    /// a call expression.
    MathLoad,
    /// Guarded `Math.<method>(args...)` intrinsic call. Operands:
    /// `Register(dst), ConstIndex(method_id), ConstIndex(argc),
    /// Register(arg0)..`. The VM/JIT may use the typed method id for
    /// direct numeric dispatch when the global `Math` method still
    /// points at the bootstrap native; otherwise it falls back to the
    /// ordinary method-call semantics so user shadows remain visible.
    MathCall,
    /// `r<dst> = trailing-args-as-array`. Operand: `Register(dst)`.
    /// Reads the call's overflow argument list (the values past
    /// the declared `param_count`) that the dispatcher stashed on
    /// the current frame and materialises them as a fresh
    /// `JsArray`. Emitted by the compiler for the `...rest`
    /// parameter at function entry.
    CollectRest,
    /// Return `r<src>` from the current function. Reuses
    /// [`Op::Return`] semantics in `<main>`; in nested calls the
    /// dispatcher pops the frame and writes the value into the
    /// caller's `return_register`.
    ReturnValue,
    /// Return `undefined` from the current function. Convenience
    /// emitted at fall-through end of function bodies.
    ReturnUndefined,

    /// `r<dst> = new JsObject()`. Operand: `dst`.
    NewObject,
    /// `r<dst> = r<obj>.<name>`. Operands: `dst, obj, name_const`.
    /// Missing property reads as `undefined`. Non-object receivers
    /// raise `TypeMismatch`.
    LoadProperty,
    /// `r<obj>.<name> = r<src>`. Operands:
    /// `obj, name_const, src, scratch_dst`.
    ///
    /// The fourth operand reserves a register that the runtime
    /// uses as the throwaway destination when an accessor setter is
    /// invoked on the receiver's prototype chain — the setter's
    /// completion value is discarded by §10.1.9 OrdinarySet, but the
    /// dispatch loop's `invoke` helper needs a register slot to
    /// write the return into. Data-property writes leave the slot
    /// untouched.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinaryset>
    StoreProperty,
    /// `r<dst> = delete r<obj>.<name>` (boolean result).
    /// Operands: `dst, obj, name_const`.
    DeleteProperty,
    /// `r<dst> = Object.getPrototypeOf(r<obj>)`. Operands:
    /// `dst, obj`. Returns `null` when no prototype is set;
    /// raises `TypeMismatch` for non-object receivers.
    GetPrototype,
    /// `Object.setPrototypeOf(r<obj>, r<proto>)`. Operands:
    /// `obj, proto`. `proto` may be a `Value::Object` or
    /// `Value::Null`. Other types raise `TypeMismatch`.
    SetPrototype,
    /// Build a fresh dense array from `elem_count` register
    /// operands. Operands: `dst, count, elem0, elem1, …`.
    NewArray,
    /// `r<dst> = r<arr>[r<idx>]`. Operands: `dst, arr, idx`.
    /// `arr` must be `Value::Array`; `idx` must be `Value::Number`
    /// in `[0, u32::MAX]` (truncates to `u32`).
    LoadElement,
    /// `r<arr>[r<idx>] = r<src>`. Operands: `arr, idx, src`.
    StoreElement,
    /// `r<dst> = r<arr>.length`. Operands: `dst, arr`.
    ArrayLength,
    /// `r<dst> = (r<lhs> in r<rhs>)`. Operands:
    /// `Register(dst), Register(lhs), Register(rhs)`.
    ///
    /// Implements ECMA-262 §13.10.1 (`RelationalExpression in
    /// ShiftExpression`). The right operand must be an Object;
    /// otherwise the dispatcher raises `TypeMismatch` (eventually
    /// `TypeError`). The left operand is coerced via §7.1.19
    /// ToPropertyKey: strings stay as-is, symbols stay as-is,
    /// numbers / booleans / null / undefined coerce to their display
    /// string. The runtime walks own + prototype-chain entries for
    /// the resolved key and writes a Boolean.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-relational-operators-runtime-semantics-evaluation>
    /// - <https://tc39.es/ecma262/#sec-hasproperty>
    HasProperty,
    /// `r<dst> = (r<lhs> instanceof r<rhs>)`. Operands:
    /// `dst, lhs, rhs`. Foundation slice 19 semantics:
    ///
    /// - `rhs` carries a `prototype` property (set later by class
    ///   lowering): the runtime walks `lhs`'s prototype chain
    ///   looking for `rhs.prototype`.
    /// - When `rhs` is itself a plain object, the runtime treats
    ///   it as the "prototype to find" and walks `lhs`'s chain
    ///   looking for it directly. This keeps the opcode useful
    ///   before classes land.
    /// - Anything else returns `false`.
    Instanceof,

    /// `r<dst> = eval(r<source>)` — indirect eval. Operands:
    /// `Register(dst), Register(source_reg)`.
    ///
    /// The runtime parses + compiles the source string as a fresh
    /// script, runs `<main>` to completion, and writes the program's
    /// completion value (or `undefined` when the script ended on a
    /// non-expression statement) into `dst`. Foundation runs eval'd
    /// code in a fresh global scope per ECMA-262 §19.4.1.1 indirect-
    /// eval semantics — direct-eval access to caller-local bindings
    /// is intentionally not supported.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-eval-x>
    Eval,
    /// `r<dst> = (r<val> is the original %eval% intrinsic)`. Operands:
    /// `dst, val`. Lets a syntactic `eval(...)` site decide at runtime
    /// whether the resolved callee is the real `eval` (→ direct eval) or
    /// a shadowing value (→ an ordinary call), per §sec-function-calls
    /// step 6.a `SameValue(func, %eval%)`.
    IsEvalIntrinsic,

    /// `r<dst> = new Function(arg0, arg1, …, body)`. Operands:
    /// `Register(dst), ConstIndex(argc), Register(arg0), …`.
    ///
    /// Variadic. The runtime stringifies each argument; the leading
    /// `argc - 1` strings become the function's parameter list and
    /// the trailing string becomes the body. The result is a
    /// callable closure-less `Value::Function` (no captures from
    /// the caller's lexical environment, per §20.2.1.1).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
    NewFunction,

    /// `r<dst> = globalThis`. Operand: `Register(dst)`. Returns the
    /// per-Interpreter shared `globalThis` JsObject. The foundation
    /// surface is intentionally minimal (the global functions are
    /// reached through dedicated opcodes; the value is mostly used
    /// for identity comparisons + `globalThis.foo = x` patterns).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-globalthis>
    LoadGlobalThis,

    /// `r<dst> = globalThis[const<name>]` with §10.2.4.1
    /// ResolveBinding semantics: throw `ReferenceError` when the
    /// global has no own property under that name. Operands:
    /// `Register(dst), ConstIndex(name)`.
    ///
    /// The compiler emits this for free-identifier reads that did
    /// not resolve to a local / upvalue / module import / known
    /// intrinsic. Strict-mode behaviour (every test262 test runs
    /// strict) is the only mode the foundation supports.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-resolvebinding>
    /// - <https://tc39.es/ecma262/#sec-getvalue>
    LoadGlobalOrThrow,

    /// `r<dst> = arguments` — materialise the current frame's
    /// incoming argument list as an arguments object. Operand:
    /// `Register(dst)`. The dispatcher populates `frame.incoming_args`
    /// at call entry only when the callee was compiled with
    /// `needs_arguments = true`, so this opcode is only emitted
    /// inside such functions. The function metadata decides whether
    /// the object is strict/unmapped or sloppy/simple mapped.
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-arguments-exotic-objects>
    /// (strict functions, non-simple parameter lists, and arrows use
    /// the unmapped variant; sloppy simple-parameter functions may
    /// expose mapped indexed properties).
    CollectArguments,

    /// `r<dst> = globalThis[const<name>]` returning `undefined` when
    /// the binding does not exist. Operands:
    /// `Register(dst), ConstIndex(name)`.
    ///
    /// Used by `typeof` on a free identifier so that
    /// `typeof Float16Array === "undefined"` evaluates to `true` per
    /// §13.5.3 (IsUnresolvableReference returns "undefined" rather
    /// than throwing).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-typeof-operator>
    LoadGlobalOrUndefined,

    /// Define/update a global `var` binding on `globalThis`.
    /// Operands: `ConstIndex(name), Register(value)`.
    ///
    /// This is distinct from `StoreProperty`: global declaration
    /// instantiation creates an own property on the global object and
    /// therefore ignores non-writable inherited properties.
    DefineGlobalVar,

    /// `r<dst> = import.meta.resolve(r<spec>)`. Operands:
    /// `Register(dst), Register(specifier_reg)`.
    ///
    /// Resolves `specifier` against the active frame's
    /// `module_url` and writes the resulting absolute URL string.
    /// Foundation supports the relative-path form
    /// (`./foo` / `../bar`) and the absolute / `https://` /
    /// `file://` pass-through case; bare specifiers fall through
    /// to the loader's resolver when one is wired.
    ///
    /// # See also
    /// - <https://html.spec.whatwg.org/multipage/webappapis.html#hostmetagetimportmetaproperties>
    ImportMetaResolve,

    /// `r<dst> = await import(r<spec>)` — runtime-resolved dynamic
    /// import. Operands: `Register(dst), Register(specifier_reg)`.
    ///
    /// Reads the string in `specifier_reg`, looks the result up
    /// against the active frame's module URL in the linker-built
    /// resolution table, and writes the target module's `module_env`
    /// into `dst`. Specifiers that aren't in the resolution table
    /// raise a `TypeError` (the foundation does not yet support
    /// re-entrant parse / compile / link for runtime-discovered
    /// modules — that's filed as part of the loader follow-up).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-import-call-runtime-semantics-evaluation>
    ImportNamespaceDynamic,

    /// Resolve a module specifier to its `module_env` object.
    /// Operands: `Register(dst), ConstIndex(specifier_const)`.
    ///
    /// The constant pool slot at `specifier_const` is a
    /// [`Constant::String`] holding the raw specifier text from
    /// the source (`"./other.ts"`, `"@scope/x"`, etc.). At runtime
    /// the dispatcher resolves it against the **caller frame's
    /// module URL** (see [`Frame::module_url`] in the VM crate)
    /// using the linker's pre-built specifier → URL table, then
    /// writes `Value::Object(module_env)` into `dst`.
    ///
    /// Used by:
    /// - The `<entry>` synthesised driver to allocate fresh
    ///   `module_env` objects per module before invoking each
    ///   `<module-init>`.
    /// - `import * as ns from "./other.ts"` lowering — the load
    ///   binds `ns` to the source module's `module_env` directly.
    /// - `import { x } from "./other.ts"` lowering — the compiler
    ///   loads the source `module_env` once at the top of the
    ///   importing module's body, hoists the result into a captured
    ///   upvalue, then emits per-reference `LoadProperty` against
    ///   it. Live bindings fall out: every read traverses the
    ///   exporter's current `module_env.x`.
    /// - `import("./literal.ts")` (literal-string dynamic import).
    ///   The slice rejects non-literal `import(expr)` at compile
    ///   time so this is the only dynamic-import path the runtime
    ///   needs to support today.
    ImportNamespace,
    /// Resolve the *deferred* namespace object for a module imported
    /// via `import defer * as ns from "x"` and write it to `r<dst>`.
    /// Operands: `Register(dst), ConstIndex(specifier)`. Unlike
    /// [`Op::ImportNamespace`], the target module's body is **not**
    /// evaluated here; the returned exotic object triggers evaluation
    /// on first access (TC39 import defer).
    ImportNamespaceDeferred,
    /// Evaluate the module with the canonical URL in the constant
    /// operand (and its not-yet-evaluated, non-deferred dependency
    /// closure, in post-order). Idempotent: a module whose body has
    /// already run is skipped. Operands: `Register(dst),
    /// ConstIndex(url)`. Emitted by
    /// the synthesised `<entry>` driver in place of an inline
    /// module-init call so eager evaluation and deferred force-eval
    /// share one guarded primitive.
    EvaluateModule,
    /// Mark the module with the canonical URL in the constant operand as
    /// evaluated, without running it. Operand: `ConstIndex(url)`. Emitted
    /// by the async `<entry>` driver around an explicit module-init call
    /// so deferred force-eval treats the module as already evaluated and
    /// does not re-run it.
    MarkModuleEvaluated,
    /// Star re-export: copy each enumerable own **string** key of the
    /// source module environment `r<src>` onto the target module
    /// environment `r<target>`, excluding `"default"` and any key the
    /// target already owns. Operands: `Register(target), Register(src)`.
    ///
    /// Lowers `export * from "mod"` (§16.2.3.7 GetExportedNames star
    /// expansion) in the copy-at-evaluation module model: explicit
    /// local exports take precedence (skip-if-present), and `default`
    /// is never propagated through a star re-export.
    StarReexport,
    /// Resolve the Module Namespace Exotic Object (ECMA-262 §10.4.6)
    /// for the module imported via `specifier` and write it to
    /// `r<dst>`. Operands: `Register(dst), ConstIndex(specifier)`.
    /// Distinct from [`Op::ImportNamespace`], which yields the raw
    /// module environment used for named-import indirection; this
    /// yields the exotic object bound by `import * as ns` /
    /// `export * as ns`.
    ModuleNamespaceObject,
    /// Read named import binding `name` from the module environment in
    /// `r<record>` and write it to `r<dst>`. Operands:
    /// `Register(dst), Register(record), ConstIndex(name)`. Unlike
    /// [`Op::LoadProperty`], a binding still in its TDZ (the slot holds
    /// the hole) raises a `ReferenceError` (§9.1.1.5 GetBindingValue).
    LoadImportBinding,
    /// Wrap the value in `r<src>` as a fulfilled `Promise` and
    /// write to `r<dst>`. Operands:
    /// `Register(dst), Register(src)`.
    ///
    /// Used by the literal-string dynamic-import lowering: after
    /// [`Op::ImportNamespace`] resolves the namespace synchronously
    /// the compiler wraps the result in a pre-fulfilled promise so
    /// the surface matches `import("./x").then(ns => ...)`.
    PromiseFulfilledOf,

    /// `r<dst> = Temporal.<member>` (read accessor). Operand pair:
    /// `Register(dst), ConstIndex(member)`.
    TemporalLoad,
    /// Build a fresh `Map` / `Set` / `WeakMap` / `WeakSet`. Operands:
    /// `Register(dst), ConstIndex(kind_const), Register(iterable)`.
    /// `kind_const` is a string constant naming the collection kind
    /// (`"Map"`, `"Set"`, `"WeakMap"`, `"WeakSet"`); `iterable` is
    /// either `Value::Undefined` (no seed) or a `Value::Array` whose
    /// elements seed the collection. For `Map` / `WeakMap` each
    /// seed element must itself be a 2-element array `[key, value]`;
    /// for `Set` / `WeakSet` each element is added directly.
    NewCollection,
    /// Build a fresh `WeakRef`. Operands:
    /// `Register(dst), Register(target)`.
    ///
    /// The runtime validates that `target` can be held weakly by the
    /// current migrated GC value model.
    NewWeakRef,
    /// Build a fresh `FinalizationRegistry`. Operands:
    /// `Register(dst), Register(cleanup_callback)`.
    ///
    /// The runtime validates that `cleanup_callback` is callable.
    NewFinalizationRegistry,
    /// `r<dst> = Symbol.<name>` (well-known symbol read). Operands:
    /// `Register(dst), ConstIndex(name)`. Lowered by the compiler
    /// for static-member access on the `Symbol` namespace; the
    /// runtime resolves `<name>` against the well-known table per
    /// ECMA-262 §6.1.5.1.
    SymbolLoad,
    /// `r<dst> = typeof r<src>`. Operands:
    /// `Register(dst), Register(src)`. Returns one of `"undefined"`,
    /// `"object"`, `"boolean"`, `"number"`, `"bigint"`, `"string"`,
    /// `"symbol"`, `"function"` per ECMA-262 §13.5.3.
    TypeOf,
    /// `r<dst> = delete r<obj>[r<idx>]` (boolean result). Operands:
    /// `dst, obj, idx`. Indexed counterpart of
    /// [`Op::DeleteProperty`]; symbol- and string-keyed objects
    /// route to the matching delete path based on the value of
    /// `idx`.
    DeleteElement,
    /// Suspend the current async frame on the awaited value. Operands:
    /// `Register(dst), Register(src)`.
    ///
    /// The dispatcher reads `src`, wraps a non-promise value as
    /// `Promise.resolve(value)`, parks the current frame off the
    /// active stack, and attaches resume / reject handlers to the
    /// awaited promise. When the awaited promise fulfils, the
    /// resume handler enqueues an internal microtask that re-pushes
    /// the parked frame onto a fresh stack, writes the resolved
    /// value into `dst`, and continues from the next pc. Rejection
    /// re-enters the parked frame and immediately throws the
    /// rejection reason, threading through any in-frame `try`/
    /// `catch`/`finally` handlers exactly as a synchronous throw
    /// would.
    ///
    /// Only legal inside a function whose
    /// [`Function::is_async`] flag is `true`; the compiler enforces
    /// this. The dispatcher does not validate the flag — the
    /// runtime simply uses the surrounding frame's async-state to
    /// decide where the suspension point's settlement lands.
    Await,
    /// `r<dst> = Object.is(r<x>, r<y>)`. Operands:
    /// `Register(dst), Register(x), Register(y)`. Dispatches to
    /// ECMA-262 §7.2.11 `SameValue` via
    /// [`otter_vm::abstract_ops::same_value`]. Distinguishes
    /// `+0` / `-0` and treats `NaN` as equal to itself.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-samevalue>
    /// - <https://tc39.es/ecma262/#sec-object.is>
    SameValue,
    /// `r<dst> = Array.isArray(r<src>)`. Operands:
    /// `Register(dst), Register(src)`. Dispatches to ECMA-262
    /// §7.2.2 `IsArray`. Today the runtime checks the `Value::Array`
    /// tag directly; once Proxy lands the dispatcher walks the
    /// proxy-target chain.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-isarray>
    /// - <https://tc39.es/ecma262/#sec-array.isarray>
    IsArray,
    /// `r<dst> = (r<x> == r<y>)`. Operands:
    /// `Register(dst), Register(x), Register(y)`. Implements
    /// ECMA-262 §7.2.13 `IsLooselyEqual` over operands the
    /// compiler has already coerced through
    /// `Op::ToPrimitive(default)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-islooselyequal>
    LooseEqual,
    /// `r<dst> = (r<x> != r<y>)`. Operands:
    /// `Register(dst), Register(x), Register(y)`. Negation of
    /// [`Self::LooseEqual`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-islooselyequal>
    LooseNotEqual,
    /// `r<dst> = new <Kind>(r<msg>)`. Operands:
    /// `Register(dst), ConstIndex(kind_const), Register(msg)`.
    ///
    /// `kind_const` references a [`Constant::String`] whose value
    /// is the canonical class name (`"Error"` /
    /// `"TypeError"` / `"RangeError"` / `"SyntaxError"` /
    /// `"ReferenceError"` / `"URIError"` / `"EvalError"`). The
    /// runtime allocates a fresh `JsObject`, links its
    /// `[[Prototype]]` to the kind's prototype from the
    /// interpreter's [`ErrorClassRegistry`], and stamps an own
    /// `message` property when `msg` is not `undefined`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    /// - <https://tc39.es/ecma262/#sec-native-error-types-used-in-this-standard>
    NewBuiltinError,
    /// `r<dst> = <Kind>` (the constructor object for one of the
    /// seven canonical error classes). Operands:
    /// `Register(dst), ConstIndex(kind_const)`.
    ///
    /// Used by the compiler when a source identifier names one of
    /// the canonical classes (e.g. `e instanceof TypeError`). The
    /// runtime fetches the constructor object from the
    /// interpreter's [`ErrorClassRegistry`]; the constructor
    /// carries a `prototype` own property so `Op::Instanceof`
    /// resolves correctly.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-error-objects>
    LoadBuiltinError,
    /// `r<dst> = String(args...)` / `String.<name>(args...)`.
    /// Operands: `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`.
    ///
    /// Variadic. Empty `name` (sentinel) selects the constructor
    /// form (coerces argument via §7.1.17 ToString); otherwise the
    /// runtime dispatches `fromCharCode` / `fromCodePoint` via
    /// `r<dst> = BigInt(args...)` / `BigInt.<name>(args...)`.
    /// Operands: `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`.
    ///
    /// Variadic. Empty `name` (sentinel) selects the constructor
    /// form; otherwise the runtime dispatches against
    /// [`crate::bigint::dispatch::call`] for `asIntN` / `asUintN`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-bigint-constructor>
    BigIntCall,

    /// `r<dst> = Array(args...)` / `r<dst> = new Array(args...)`.
    /// Operands: `Register(dst), ConstIndex(argc), Register(arg0), …`.
    ///
    /// §23.1.1.1 — single-numeric argument reserves a sparse array
    /// of that length; everything else collects values verbatim.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array>
    ArrayConstruct,
    /// `r<dst> = Array.from(args...)`. Operands:
    /// `Register(dst), ConstIndex(argc), Register(arg0), …`.
    ///
    /// §23.1.2.1 Array.from. Variadic.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.from>
    ArrayFrom,
    /// `r<dst> = Array.of(args...)`. Operands:
    /// `Register(dst), ConstIndex(argc), Register(arg0), …`.
    ///
    /// §23.1.2.3 Array.of. Variadic.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-array.of>
    ArrayOf,
    /// `r<dst> = enumerable-string-keys(r<obj>)` — internal `for-in`
    /// snapshot. Operands: `Register(dst), Register(obj)`.
    ///
    /// §14.7.5.6 EnumerateObjectProperties: collect the enumerable
    /// own + inherited String-typed property keys of `obj` into a
    /// fresh array, walking the prototype chain and de-duplicating.
    /// The dispatcher uses the same helper that backs `for (k in o)`
    /// — it is **not** an alias for `Object.keys`, which is own-only.
    ///
    /// Spec primitive: emitted only by the compiler for `for-in`
    /// lowering. Not user-observable as a method name and therefore
    /// not interceptable by shadowing `Object.<anything>`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-enumerate-object-properties>
    /// - <https://tc39.es/ecma262/#sec-for-in-and-for-of-statements>
    ForInKeys,
    /// `r<target>` ←` enumerable own properties of r<src>`. Operands:
    /// `Register(target), Register(src)`.
    ///
    /// §7.3.31 CopyDataProperties: copy each enumerable own string-
    /// (and symbol-) keyed property of `src` onto `target` via
    /// `[[Set]]`. Null / undefined sources are no-ops. Spec primitive
    /// used by the compiler to lower `{ ...source }` object spread
    /// and rest binding patterns. Routes around any user shadow of
    /// `Object.assign`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-copydataproperties>
    CopyDataProperties,
    /// `r<target>.[[DefineOwnProperty]](r<key>, r<desc>)`. Operands:
    /// `Register(target), Register(key), Register(desc)`.
    ///
    /// §10.1.6.1 OrdinaryDefineOwnProperty applied with the runtime
    /// descriptor object referenced by `desc` (read through full
    /// `[[Get]]` per §6.2.5.5 ToPropertyDescriptor). Spec primitive
    /// emitted by the compiler for class method installation and
    /// computed-key property definition; bypasses user shadows of
    /// `Object.defineProperty`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-ordinarydefineownproperty>
    /// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
    DefineOwnProperty,
    /// `r<dst> = ArrayBuffer(args...)` / `ArrayBuffer.<name>(args...)`.
    /// Operands: `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`.
    ///
    /// Empty `name` selects the constructor (§25.1.4.1
    /// `ArrayBuffer(length, options?)`); otherwise the runtime
    /// dispatches `isView` per §25.1.4.3 against
    /// [`crate::binary::dispatch::array_buffer_call`].
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
    ArrayBufferCall,
    /// `r<dst> = new DataView(args...)`. Operands:
    /// `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`.
    ///
    /// Empty `name` selects the constructor (§25.3.1.1
    /// `DataView(buffer, byteOffset?, byteLength?)`). DataView has
    /// no spec-defined static methods; non-empty `name` raises
    /// `UnknownIntrinsic`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-dataview-constructor>
    DataViewCall,
    /// `yield r<src>` inside a generator body — pause the running
    /// frame and surface `r<src>` to the caller's `.next()` /
    /// iteration step. Operands: `Register(dst), Register(src)`.
    ///
    /// `dst` receives the value passed to the matching `.next(arg)`
    /// (or `undefined` when the iterator was driven by a `for-of`
    /// loop with no explicit argument). When the generator is
    /// resumed via `.throw(err)`, the dispatcher routes `err`
    /// through the surrounding handler stack rather than writing
    /// it into `dst`.
    ///
    /// Only legal inside a function whose
    /// [`Function::is_generator`] flag is `true`; the compiler
    /// enforces that.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-yield>
    Yield,
    /// `r<dst> = SharedArrayBuffer(args...)`. Operands:
    /// `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`.
    ///
    /// Empty `name` selects the §25.2.1 constructor; otherwise
    /// dispatches `SharedArrayBuffer.<static>` (none today;
    /// reserved).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
    SharedArrayBufferCall,
    /// `r<dst> = ToPrimitive(r<src>, hint)`. Operands:
    /// `Register(dst), Register(src), ConstIndex(hint_const)`.
    ///
    /// `hint_const` references a [`Constant::String`] holding one
    /// of `"default"`, `"number"`, or `"string"` per §7.1.1 step 4.
    ///
    /// Already-primitive `src` short-circuits: the dispatcher writes
    /// `src` to `dst` and advances pc. Otherwise the dispatcher
    /// drives a multi-stage ladder over `[Symbol.toPrimitive]` and
    /// the OrdinaryToPrimitive `valueOf` / `toString` chain. The
    /// stages are tracked on the active frame's
    /// [`Frame::pending_to_primitive`](otter_vm::Frame) slot; pc
    /// stays on this opcode until the ladder produces a primitive
    /// value (or every stage is exhausted, in which case the
    /// dispatcher raises `TypeMismatch`).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toprimitive>
    /// - <https://tc39.es/ecma262/#sec-ordinarytoprimitive>
    ToPrimitive,

    /// Declare a global `var` binding if absent. Operands:
    /// `ConstIndex(name), Imm32(configurable)`.
    ///
    /// §9.1.1.4.17 CreateGlobalVarBinding — when `globalThis` has no
    /// own property `name` (and is extensible), define a writable /
    /// enumerable / configurable data property initialised to
    /// `undefined`. An existing own property is left untouched, so
    /// re-declaration (sibling scripts, Annex B extensions over a
    /// pre-existing global) never resets a live value.
    DeclareGlobalVar,

    /// Read an identifier that may resolve to an eval-introduced
    /// caller binding. Operands: `Register(dst), ConstIndex(name)`.
    ///
    /// §9.1.2.1 GetIdentifierReference over a function environment a
    /// direct eval extended at runtime: consult the frame's
    /// eval-introduced binding map first, then fall back to the
    /// global environment (`ReferenceError` when absent, like
    /// [`Op::LoadGlobalOrThrow`]). Emitted only inside functions
    /// whose body contains a direct eval call site.
    LoadDynamic,

    /// Write an identifier that may resolve to an eval-introduced
    /// caller binding. Operands: `Register(value), ConstIndex(name)`.
    ///
    /// §10.2.4.2 PutValue counterpart of [`Op::LoadDynamic`]: store
    /// through the frame's eval-introduced binding when present,
    /// else fall back to the sloppy-mode `globalThis` property
    /// write.
    StoreDynamic,

    /// `typeof` flavour of [`Op::LoadDynamic`]. Operands:
    /// `Register(dst), ConstIndex(name)`. An unresolvable name
    /// yields `undefined` instead of throwing (§13.5.3).
    TypeofDynamic,

    /// `delete` flavour of [`Op::LoadDynamic`] — §19.2.1.3 eval-created
    /// var bindings are deletable. Operands:
    /// `Register(dst), ConstIndex(name)`. Removes the binding from the
    /// frame's eval-var map / captured eval-env chain (`true`), else
    /// falls through to the global-object delete.
    DeleteDynamic,

    /// Mint a Private Name carrier (§6.2.12) — a symbol whose
    /// `private_name` marker keeps it out of Proxy traps and arms
    /// the §7.3.28 extensibility check. Operands:
    /// `Register(dst), ConstIndex(description)`.
    NewPrivateName,

    /// §9.1.1.4.18 CreateGlobalFunctionBinding. Operands:
    /// `ConstIndex(name), Register(value), Imm32(deletable)`.
    ///
    /// An absent or configurable existing own property is redefined
    /// as `{value, writable: true, enumerable: true, configurable:
    /// deletable}`; a non-configurable existing property must be a
    /// writable + enumerable data property (else `TypeError`,
    /// §9.1.1.4.16 CanDeclareGlobalFunction) and only receives the
    /// new value. Scripts pass `deletable = 0`, eval bodies `1`.
    DefineGlobalFunction,

    /// §9.1.1.4 CreateMutableBinding / CreateImmutableBinding on the
    /// global *declarative* record. Operands: `ConstIndex(name),
    /// Imm32(is_const)`.
    ///
    /// Validates §16.1.7 steps 4–5 first: an existing global lexical
    /// of the same name, a global `[[VarNames]]` entry, or a
    /// restricted global property (existing non-configurable own
    /// property of the global object) raises `SyntaxError`. The
    /// fresh cell starts as the TDZ hole.
    DeclareGlobalLex,

    /// §9.1.1.4 global-environment `SetMutableBinding`. Operands:
    /// `Register(value), ConstIndex(name), Imm32(strict)`.
    ///
    /// The declarative record is consulted first (`const` →
    /// `TypeError`, TDZ hole → `ReferenceError`, else cell write),
    /// then the object record: an existing property receives the
    /// value; an absent one is a `ReferenceError` in strict mode and
    /// a fresh global property otherwise.
    StoreGlobalBinding,

    /// §9.1.1.4 global-declarative `InitializeBinding` — write the
    /// initializer value through a lexical cell created by
    /// [`Op::DeclareGlobalLex`], clearing its TDZ hole. Operands:
    /// `Register(value), ConstIndex(name)`.
    InitGlobalLex,

    /// §16.1.7 GlobalDeclarationInstantiation steps 1–12 /
    /// §19.2.1.3 steps 5–11 — validate one declared name against the
    /// global environment *before any binding is created*, so a
    /// failing script instantiates nothing. Operands:
    /// `ConstIndex(name), Imm32(kind)` where kind 0 = lexical
    /// (SyntaxError on an existing lexical, `[[VarNames]]` entry, or
    /// restricted global property), 1 = var (SyntaxError on an
    /// existing lexical; TypeError when a fresh binding cannot be
    /// created on a non-extensible global), 2 = function (SyntaxError
    /// on an existing lexical; TypeError per §9.1.1.4.16
    /// CanDeclareGlobalFunction).
    ValidateGlobalDecl,

    /// `r<dst> = ToObject(r<src>)` per §7.1.18. Primitives wrap in
    /// their `%X.prototype%` body with the matching internal data
    /// slot; objects pass through unchanged; `null` / `undefined`
    /// throw a TypeError. Operands: `Register(dst), Register(src)`.
    ///
    /// Emitted by the `with` statement lowering (§14.11.2 step 2)
    /// so a primitive scope expression resolves identifier lookups
    /// against its wrapper object.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-toobject>
    ToObject,

    /// `r<dst> = ToNumeric(r<src>)` per §7.1.3, for an operand that
    /// is already primitive: Number and BigInt pass through,
    /// String / Boolean / null / undefined convert via ToNumber,
    /// and a Symbol throws TypeError. Emitted between the two
    /// `ToPrimitive` coercions of a numeric binary operator so
    /// `ToNumeric(lhs)` completes (and throws) before the right
    /// operand's `valueOf` runs (§13.15.3 ApplyStringOrNumeric
    /// BinaryOperator evaluation order). Operands:
    /// `Register(dst), Register(src)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-tonumeric>
    ToNumeric,

    /// `r<dst> = PrivateGet(r<obj>, r<key>)` per §7.3.31. `key`
    /// holds the class-evaluation private-name symbol. Resolves the
    /// private element on the receiver (own fields) or its
    /// prototype chain (methods / accessors): absent name throws
    /// TypeError (brand check), an accessor without a getter throws
    /// TypeError, an accessor with one invokes it with the receiver
    /// as `this`. Operands: `Register(dst), Register(obj),
    /// Register(key)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-privateget>
    PrivateGet,

    /// `PrivateSet(r<obj>, r<key>, r<value>)` per §7.3.32. Absent
    /// private name throws TypeError (brand check); a private
    /// method (data element found on the prototype side) throws
    /// TypeError; an accessor without a setter throws TypeError; an
    /// accessor with one invokes it; an own field writes in place.
    /// Operands: `Register(obj), Register(key), Register(value)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-privateset>
    PrivateSet,

    /// `yield*` delegating suspension per §27.5.3.7. Parks the
    /// generator frame with `r<src>` (the inner iterator result
    /// object) as the value surfaced verbatim from the outer
    /// `.next()`. On resume the runtime writes the resume kind code
    /// into `r<kind_dst>` (0 = next, 1 = throw, 2 = return) and the
    /// resume argument into `r<value_dst>` — abrupt resumes do NOT
    /// unwind the body; the compiled delegation loop forwards them
    /// to the inner iterator's `throw` / `return` method. Operands:
    /// `Register(kind_dst), Register(value_dst), Register(src)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-function-definitions-runtime-semantics-evaluation>
    YieldDelegate,

    /// `CreateDataPropertyOrThrow(r<obj>, ToPropertyKey(r<key>),
    /// r<value>)` per §7.3.7 — defines an own
    /// `{writable, enumerable, configurable: true}` data property
    /// WITHOUT consulting setters on the prototype chain. Object
    /// literal property definitions lower through this (a
    /// `StoreProperty` would observably fire inherited setters,
    /// e.g. `Object.prototype.__proto__`). Operands:
    /// `Register(obj), Register(key), Register(value)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-createdatapropertyorthrow>
    DefineDataProperty,

    /// `SetFunctionName(r<fn>, r<key>, prefix)` per §10.2.10 — names
    /// an anonymous function from a run-time property key: a Symbol
    /// key gives `"[description]"` (or `""` for a description-less
    /// symbol), anything else coerces to string; a non-empty prefix
    /// (`"get"` / `"set"`) prepends with a space. Operands:
    /// `Register(fn), Register(key), ConstIndex(prefix)`.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-setfunctionname>
    SetFunctionName,

    /// Class-definition runtime validation. Operands:
    /// `Imm32(kind), Register(reg)`.
    ///
    /// - kind 0 — §15.7.14 ClassDefinitionEvaluation step 6.f:
    ///   `extends` heritage must be `null` or a constructor
    ///   (arrows, generators, async functions and plain objects
    ///   throw TypeError before any prototype read).
    /// - kind 1 — §15.7.14 step 22 / ClassElementEvaluation: a
    ///   static element's computed key must not be `"prototype"`
    ///   (TypeError).
    ClassCheck,
    /// `ToPropertyKey dst, src` — §7.1.19 with full user coercion
    /// (`@@toPrimitive` / `valueOf` / `toString`): `dst` receives the
    /// canonical property key as a Symbol or String value. Class
    /// field definitions evaluate their computed names through this
    /// at class-definition time.
    ToPropertyKey,
    /// `Increment dst, src, delta` — §13.4.2 UpdateExpression
    /// numeric step: `dst = ToNumeric(src) + delta` where `delta` is
    /// the Imm32 `+1` / `-1`. BigInt operands stay BigInt
    /// (§6.1.6.2.7), Numbers fold as f64.
    Increment,
    /// `PrivateBrandCheck obj, brand` — §7.3.31 PrivateElementFind
    /// own-only step for private METHODS / ACCESSORS: the receiver
    /// must carry the class's own brand marker (installed by
    /// InitializeInstanceElements after `super()` returns); a
    /// missing brand throws TypeError. Fields skip this (their own
    /// store lookup already fails).
    PrivateBrandCheck,
    /// `LoadShadowedUpvalue dst, name_const, upvalue_idx` — read a
    /// captured binding in a function whose body contains a direct
    /// eval: an eval-introduced var of the SAME name shadows the
    /// capture (§9.1 — the eval declared it in this frame's variable
    /// environment, which sits inner to the captured one).
    LoadShadowedUpvalue,
    /// `GetTemplateObject dst, site_const` — §13.2.8.4: the frozen
    /// (with frozen non-enumerable `.raw`) template-strings object
    /// for tagged-template site `site_const`, cached realm-wide per
    /// site.
    GetTemplateObject,
}

impl Op {
    /// Canonical mnemonic spelling for disassembly and trace events.
    #[must_use]
    pub const fn mnemonic(self) -> &'static str {
        match self {
            Op::Nop => "NOP",
            Op::LoadUndefined => "LOAD_UNDEFINED",
            Op::LoadHole => "LOAD_HOLE",
            Op::Return => "RETURN",
            Op::LoadString => "LOAD_STRING",
            Op::LoadNumber => "LOAD_NUMBER",
            Op::LoadInt32 => "LOAD_INT32",
            Op::LoadBigInt => "LOAD_BIGINT",
            Op::LoadRegExp => "LOAD_REGEXP",
            Op::QueueMicrotask => "QUEUE_MICROTASK",
            Op::PromiseNew => "PROMISE_NEW",
            Op::PromiseCall => "PROMISE_CALL",
            Op::LoadTrue => "LOAD_TRUE",
            Op::LoadFalse => "LOAD_FALSE",
            Op::LoadLength => "LOAD_LENGTH",
            Op::GetStringIndex => "GET_STRING_INDEX",
            Op::CallMethodValue => "CALL_METHOD_VALUE",
            Op::CallWithThis => "CALL_WITH_THIS",
            Op::BindFunction => "BIND_FUNCTION",
            Op::LoadThis => "LOAD_THIS",
            Op::LoadNewTarget => "LOAD_NEW_TARGET",
            Op::Throw => "THROW",
            Op::EnterTry => "ENTER_TRY",
            Op::LeaveTry => "LEAVE_TRY",
            Op::EndFinally => "END_FINALLY",
            Op::NewError => "NEW_ERROR",
            Op::GeneratorStart => "GENERATOR_START",
            Op::GetIterator => "GET_ITERATOR",
            Op::GetAsyncIterator => "GET_ASYNC_ITERATOR",
            Op::IteratorNext => "ITERATOR_NEXT",
            Op::IteratorClose => "ITERATOR_CLOSE",
            Op::IteratorCloseStart => "ITERATOR_CLOSE_START",
            Op::IteratorCloseEnd => "ITERATOR_CLOSE_END",
            Op::ArrayPush => "ARRAY_PUSH",
            Op::CallSpread => "CALL_SPREAD",
            Op::New => "NEW",
            Op::NewSpread => "NEW_SPREAD",
            Op::SuperConstructSpread => "SUPER_CONSTRUCT_SPREAD",
            Op::BindThisValue => "BIND_THIS_VALUE",
            Op::LoadSuperProperty => "LOAD_SUPER_PROPERTY",
            Op::LoadSuperElement => "LOAD_SUPER_ELEMENT",
            Op::SetSuperProperty => "SET_SUPER_PROPERTY",
            Op::SetSuperElement => "SET_SUPER_ELEMENT",
            Op::JumpViaFinally => "JUMP_VIA_FINALLY",
            Op::PopParkedFinally => "POP_PARKED_FINALLY",
            Op::GlobalBindingExists => "GLOBAL_BINDING_EXISTS",
            Op::StoreGlobalChecked => "STORE_GLOBAL_CHECKED",
            Op::MakeClass => "MAKE_CLASS",
            Op::CollectRest => "COLLECT_REST",
            Op::MathLoad => "MATH_LOAD",
            Op::MathCall => "MATH_CALL",
            Op::Add => "ADD",
            Op::Sub => "SUB",
            Op::Mul => "MUL",
            Op::Div => "DIV",
            Op::Rem => "REM",
            Op::Neg => "NEG",
            Op::Pow => "POW",
            Op::BitwiseAnd => "BIT_AND",
            Op::BitwiseOr => "BIT_OR",
            Op::BitwiseXor => "BIT_XOR",
            Op::BitwiseNot => "BIT_NOT",
            Op::Shl => "SHL",
            Op::Shr => "SHR",
            Op::Ushr => "USHR",
            Op::ToNumber => "TO_NUMBER",
            Op::Equal => "EQ",
            Op::NotEqual => "NEQ",
            Op::LessThan => "LT",
            Op::LessEq => "LE",
            Op::GreaterThan => "GT",
            Op::GreaterEq => "GE",
            Op::LoadNull => "LOAD_NULL",
            Op::LogicalNot => "NOT",
            Op::ToBoolean => "TO_BOOLEAN",
            Op::Jump => "JUMP",
            Op::JumpIfTrue => "JUMP_IF_TRUE",
            Op::JumpIfFalse => "JUMP_IF_FALSE",
            Op::JumpIfNullish => "JUMP_IF_NULLISH",
            Op::LoadLocal => "LOAD_LOCAL",
            Op::StoreLocal => "STORE_LOCAL",
            Op::TdzError => "TDZ_ERROR",
            Op::MakeFunction => "MAKE_FUNCTION",
            Op::MakeClosure => "MAKE_CLOSURE",
            Op::LoadUpvalue => "LOAD_UPVALUE",
            Op::StoreUpvalue => "STORE_UPVALUE",
            Op::StoreUpvalueChecked => "STORE_UPVALUE_CHECKED",
            Op::FreshUpvalue => "FRESH_UPVALUE",
            Op::Call => "CALL",
            Op::TailCall => "TAIL_CALL",
            Op::ReturnValue => "RETURN_VALUE",
            Op::ReturnUndefined => "RETURN_UNDEFINED",
            Op::NewObject => "NEW_OBJECT",
            Op::LoadProperty => "LOAD_PROPERTY",
            Op::StoreProperty => "STORE_PROPERTY",
            Op::DeleteProperty => "DELETE_PROPERTY",
            Op::GetPrototype => "GET_PROTOTYPE",
            Op::SetPrototype => "SET_PROTOTYPE",
            Op::NewArray => "NEW_ARRAY",
            Op::LoadElement => "LOAD_ELEMENT",
            Op::StoreElement => "STORE_ELEMENT",
            Op::ArrayLength => "ARRAY_LENGTH",
            Op::Instanceof => "INSTANCEOF",
            Op::ImportNamespace => "IMPORT_NAMESPACE",
            Op::ImportNamespaceDeferred => "IMPORT_NAMESPACE_DEFERRED",
            Op::EvaluateModule => "EVALUATE_MODULE",
            Op::MarkModuleEvaluated => "MARK_MODULE_EVALUATED",
            Op::StarReexport => "STAR_REEXPORT",
            Op::ModuleNamespaceObject => "MODULE_NAMESPACE_OBJECT",
            Op::LoadImportBinding => "LOAD_IMPORT_BINDING",
            Op::PromiseFulfilledOf => "PROMISE_FULFILLED_OF",
            Op::Await => "AWAIT",
            Op::SymbolLoad => "SYMBOL_LOAD",
            Op::TypeOf => "TYPEOF",
            Op::DeleteElement => "DELETE_ELEMENT",
            Op::NewCollection => "NEW_COLLECTION",
            Op::NewWeakRef => "NEW_WEAK_REF",
            Op::NewFinalizationRegistry => "NEW_FINALIZATION_REGISTRY",
            Op::TemporalLoad => "TEMPORAL_LOAD",
            Op::SameValue => "SAME_VALUE",
            Op::IsArray => "IS_ARRAY",
            Op::ToPrimitive => "TO_PRIMITIVE",
            Op::LooseEqual => "LOOSE_EQ",
            Op::LooseNotEqual => "LOOSE_NEQ",
            Op::NewBuiltinError => "NEW_BUILTIN_ERROR",
            Op::LoadBuiltinError => "LOAD_BUILTIN_ERROR",
            Op::ForInKeys => "FOR_IN_KEYS",
            Op::CopyDataProperties => "COPY_DATA_PROPERTIES",
            Op::DefineOwnProperty => "DEFINE_OWN_PROPERTY",
            Op::ArrayConstruct => "ARRAY_CONSTRUCT",
            Op::ArrayFrom => "ARRAY_FROM",
            Op::ArrayOf => "ARRAY_OF",
            Op::BigIntCall => "BIGINT_CALL",
            Op::HasProperty => "HAS_PROPERTY",
            Op::ImportNamespaceDynamic => "IMPORT_NAMESPACE_DYNAMIC",
            Op::ImportMetaResolve => "IMPORT_META_RESOLVE",
            Op::LoadGlobalThis => "LOAD_GLOBAL_THIS",
            Op::LoadGlobalOrThrow => "LOAD_GLOBAL_OR_THROW",
            Op::LoadGlobalOrUndefined => "LOAD_GLOBAL_OR_UNDEFINED",
            Op::DefineGlobalVar => "DEFINE_GLOBAL_VAR",
            Op::DeclareGlobalVar => "DECLARE_GLOBAL_VAR",
            Op::LoadDynamic => "LOAD_DYNAMIC",
            Op::StoreDynamic => "STORE_DYNAMIC",
            Op::TypeofDynamic => "TYPEOF_DYNAMIC",
            Op::DeleteDynamic => "DELETE_DYNAMIC",
            Op::NewPrivateName => "NEW_PRIVATE_NAME",
            Op::DefineGlobalFunction => "DEFINE_GLOBAL_FUNCTION",
            Op::DeclareGlobalLex => "DECLARE_GLOBAL_LEX",
            Op::StoreGlobalBinding => "STORE_GLOBAL_BINDING",
            Op::InitGlobalLex => "INIT_GLOBAL_LEX",
            Op::ValidateGlobalDecl => "VALIDATE_GLOBAL_DECL",
            Op::ToObject => "TO_OBJECT",
            Op::ToNumeric => "TO_NUMERIC",
            Op::PrivateGet => "PRIVATE_GET",
            Op::PrivateSet => "PRIVATE_SET",
            Op::YieldDelegate => "YIELD_DELEGATE",
            Op::DefineDataProperty => "DEFINE_DATA_PROPERTY",
            Op::SetFunctionName => "SET_FUNCTION_NAME",
            Op::ClassCheck => "CLASS_CHECK",
            Op::ToPropertyKey => "TO_PROPERTY_KEY",
            Op::Increment => "INCREMENT",
            Op::PrivateBrandCheck => "PRIVATE_BRAND_CHECK",
            Op::LoadShadowedUpvalue => "LOAD_SHADOWED_UPVALUE",
            Op::GetTemplateObject => "GET_TEMPLATE_OBJECT",
            Op::CollectArguments => "COLLECT_ARGUMENTS",
            Op::Eval => "EVAL",
            Op::IsEvalIntrinsic => "IS_EVAL_INTRINSIC",
            Op::NewFunction => "NEW_FUNCTION",
            Op::ArrayBufferCall => "ARRAY_BUFFER_CALL",
            Op::DataViewCall => "DATA_VIEW_CALL",
            Op::Yield => "YIELD",
            Op::SharedArrayBufferCall => "SHARED_ARRAY_BUFFER_CALL",
        }
    }

    /// Declared operand arity. Some call opcodes are variadic; the
    /// instruction stream stores a fixed prefix followed by `argc`
    /// register operands. `operand_count` returns the **prefix**
    /// length; consumers walk the variadic tail by reading `argc`.
    /// `CallWithThis` and `BindFunction` follow the same convention
    /// with an extra `this` register before `argc`.
    #[must_use]
    pub const fn operand_count(self) -> usize {
        if let Some(operands) = crate::opcode_schema::opcode_schema(self)
            .operand_shape
            .prefix()
        {
            return operands.len();
        }
        match self {
            Op::Nop | Op::ReturnUndefined | Op::LeaveTry | Op::EndFinally | Op::GeneratorStart => 0,
            Op::LoadUndefined
            | Op::LoadHole
            | Op::LoadNull
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadThis
            | Op::LoadNewTarget
            | Op::Return
            | Op::ReturnValue
            | Op::Jump
            | Op::TdzError
            | Op::Throw
            | Op::NewObject
            | Op::CollectRest
            | Op::IteratorClose
            | Op::IteratorCloseStart
            | Op::IteratorCloseEnd
            | Op::CollectArguments
            | Op::FreshUpvalue
            | Op::MarkModuleEvaluated
            | Op::LoadGlobalThis => 1,
            Op::LoadString
            | Op::LoadNumber
            | Op::LoadInt32
            | Op::LoadBigInt
            | Op::LoadRegExp
            | Op::LoadLength
            | Op::Neg
            | Op::BitwiseNot
            | Op::ToNumber
            | Op::LogicalNot
            | Op::ToBoolean
            | Op::JumpIfTrue
            | Op::JumpIfFalse
            | Op::JumpIfNullish
            | Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadUpvalue
            | Op::StoreUpvalue
            | Op::StoreUpvalueChecked
            | Op::MakeFunction
            | Op::MathLoad
            | Op::Await
            | Op::IsEvalIntrinsic
            | Op::ImportNamespace
            | Op::ImportNamespaceDeferred
            | Op::ModuleNamespaceObject
            | Op::ImportNamespaceDynamic
            | Op::ImportMetaResolve
            | Op::EvaluateModule
            | Op::Eval
            | Op::PromiseFulfilledOf
            | Op::SymbolLoad
            | Op::TypeOf
            | Op::TemporalLoad
            | Op::IsArray
            | Op::LoadBuiltinError
            | Op::LoadGlobalOrThrow
            | Op::LoadGlobalOrUndefined
            | Op::DefineGlobalVar
            | Op::DeclareGlobalVar
            | Op::LoadDynamic
            | Op::StoreDynamic
            | Op::TypeofDynamic
            | Op::DeleteDynamic
            | Op::NewPrivateName
            | Op::DeclareGlobalLex
            | Op::InitGlobalLex
            | Op::ValidateGlobalDecl
            | Op::ClassCheck
            | Op::ToObject
            | Op::ToPropertyKey
            | Op::PrivateBrandCheck
            | Op::GetTemplateObject
            | Op::ToNumeric => 2,
            Op::Increment
            | Op::LoadShadowedUpvalue
            | Op::GetStringIndex
            | Op::Add
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
            | Op::GreaterEq
            | Op::LoadProperty
            | Op::DeleteProperty
            | Op::Instanceof
            | Op::HasProperty
            | Op::SameValue
            | Op::ToPrimitive
            | Op::LooseEqual
            | Op::LooseNotEqual
            | Op::LoadImportBinding
            | Op::NewBuiltinError
            | Op::DefineGlobalFunction
            | Op::PrivateGet
            | Op::PrivateSet
            | Op::YieldDelegate
            | Op::DefineDataProperty
            | Op::SetFunctionName
            | Op::StoreGlobalBinding => 3,
            Op::GetPrototype
            | Op::SetPrototype
            | Op::ArrayLength
            | Op::NewError
            | Op::GetIterator
            | Op::GetAsyncIterator
            | Op::ArrayPush
            | Op::NewWeakRef
            | Op::NewFinalizationRegistry => 2,
            Op::IteratorNext => 3,
            Op::NewCollection => 3,
            Op::CallSpread => 4,
            Op::NewSpread | Op::SuperConstructSpread => 3,
            Op::BindThisValue => 1,
            Op::LoadSuperProperty | Op::LoadSuperElement => 3,
            Op::SetSuperProperty | Op::SetSuperElement => 3,
            Op::JumpViaFinally => 2,
            Op::PopParkedFinally => 1,
            Op::GlobalBindingExists => 2,
            Op::StoreGlobalChecked => 3,
            // dst, name_const, src, scratch_dst.
            Op::StoreProperty => 4,
            // `NewArray` is variadic: `dst, count, elems...`. The
            // dispatcher reads the count and walks the trailing
            // operands.
            Op::NewArray => 2,
            Op::LoadElement | Op::DeleteElement => 3,
            // recv, key, src, scratch_dst for accessor setters.
            Op::StoreElement => 4,
            Op::CallMethodValue => 4,    // dst, recv, name_const, argc
            Op::MathCall => 3,           // dst, method_id, argc — args follow
            Op::ForInKeys => 2,          // dst, obj
            Op::CopyDataProperties => 2, // target, src
            Op::StarReexport => 2,       // target_env, src_env
            Op::DefineOwnProperty => 3,  // target, key, desc
            // dst, argc — args follow as `Register(arg0)…`.
            Op::ArrayConstruct | Op::ArrayFrom | Op::ArrayOf => 2,
            Op::BigIntCall => 3,      // dst, name_const, argc — args follow
            Op::ArrayBufferCall => 3, // dst, name_const, argc — args follow
            Op::DataViewCall => 3,    // dst, name_const, argc — args follow
            Op::Yield => 2,           // dst, src
            Op::SharedArrayBufferCall => 3, // dst, name_const, argc — args follow
            Op::NewFunction => 2,     // dst, argc — args follow
            Op::QueueMicrotask => 2,  // callee, argc — args follow
            Op::PromiseNew => 3,      // dst, executor_reg, scratch_dst
            Op::PromiseCall => 3,     // dst, name_const, argc — args follow
            Op::Call | Op::TailCall | Op::New => 3, // dst, callee, argc — args follow
            Op::MakeClass => 5,       // dst, ctor, prototype, statics, parent
            // dst, callee, this, argc — args follow.
            Op::CallWithThis | Op::BindFunction => 4,
            // catch_offset, finally_offset, exc_dst.
            Op::EnterTry => 3,
            // `MakeClosure` is variadic: `dst, function_const,
            // upvalue_count, srcs...`. The dispatcher reads the
            // count and walks the trailing operands.
            Op::MakeClosure => 3,
        }
    }

    /// Whether `operands[pos]` of this opcode is a reference into
    /// [`BytecodeModule::constants`] (vs. a raw count, immediate, or
    /// method-id value that happens to be encoded as
    /// [`Operand::ConstIndex`]).
    ///
    /// The module-graph linker uses this to decide which operand
    /// slots to offset when concatenating per-fragment constant
    /// pools into the unified [`BytecodeModule`]. Mis-classifying a
    /// raw count / method-id slot here corrupts the merged program
    /// (the `Math.abs` opcode, for example, would silently dispatch
    /// against a different `MathMethod` after fragment merge).
    ///
    /// # Operand-kind summary
    /// - **Pool ref** (this method returns `true`): the runtime
    ///   resolves the operand via
    ///   [`BytecodeModule::constants`]`[idx]` to a string, number,
    ///   bigint, regexp, or function-id constant.
    /// - **Raw count / immediate** (returns `false`): the operand
    ///   carries `argc`, `upvalue_count`, a method-id enum
    ///   (`MathMethod`, `JsonMethod`, `ObjectMethod`, …), or a
    ///   typed-array kind enum. The linker must leave these
    ///   unchanged.
    ///
    /// # See also
    /// - [`crate::Operand::ConstIndex`]
    /// - [`Op::operand_count`]
    #[must_use]
    pub const fn is_const_pool_operand(self, pos: usize) -> bool {
        match self {
            // [reg, const]
            Op::LoadString
            | Op::LoadNumber
            | Op::LoadBigInt
            | Op::LoadRegExp
            | Op::MakeFunction
            | Op::MathLoad
            | Op::ImportNamespace
            | Op::ImportNamespaceDeferred
            | Op::ModuleNamespaceObject
            | Op::SymbolLoad
            | Op::TemporalLoad
            | Op::LoadBuiltinError
            | Op::LoadGlobalOrThrow
            | Op::LoadGlobalOrUndefined
            | Op::LoadDynamic
            | Op::StoreDynamic
            | Op::TypeofDynamic
            | Op::DeleteDynamic
            | Op::NewPrivateName => pos == 1,
            // [name_const, value_reg]
            Op::DefineGlobalVar => pos == 0,
            Op::DeclareGlobalVar => pos == 0,
            // [name_const, value_reg, deletable_imm]
            Op::DefineGlobalFunction => pos == 0,
            // [name_const, is_const_imm] / [name_const, kind_imm]
            Op::DeclareGlobalLex | Op::ValidateGlobalDecl => pos == 0,
            // [value_reg, name_const(, strict_imm)]
            Op::StoreGlobalBinding | Op::InitGlobalLex => pos == 1,
            // [url_const]
            Op::MarkModuleEvaluated => pos == 0,
            // [gate_dst, url_const]
            Op::EvaluateModule => pos == 1,
            // [reg, reg, const]
            Op::LoadProperty | Op::DeleteProperty | Op::ToPrimitive => pos == 2,
            // [dst, url_const, name_const]
            Op::LoadImportBinding => pos == 1 || pos == 2,
            // [reg, kind_const, reg]
            Op::NewCollection | Op::NewBuiltinError => pos == 1,
            Op::SetFunctionName => pos == 2,
            // [reg, name_const, src_reg, scratch_dst]
            Op::StoreProperty => pos == 1,
            // [reg, function_const, count, parent_idxs...]
            Op::MakeClosure => pos == 1,
            // [reg, recv, name_const, argc, args...]
            Op::CallMethodValue => pos == 2,
            // Variadic *Call shapes whose pos=1 is a method-id enum
            // (e.g. `PromiseMethod::from_u32`), **not** a
            // constant-pool reference. The linker must NOT
            // offset these slots — doing so silently rebinds the
            // call to a different builtin method after merge.
            Op::PromiseCall
            | Op::BigIntCall
            | Op::ArrayBufferCall
            | Op::DataViewCall
            | Op::SharedArrayBufferCall => false,
            // No constant-pool refs in any other operand position.
            _ => false,
        }
    }

    /// Whether the opcode performs a control-flow transfer. The
    /// dispatcher uses this to advance `pc` by 1 only for non-jump
    /// opcodes; jumps mutate `pc` themselves (and the back-edge
    /// hook polls the interrupt flag).
    #[must_use]
    pub const fn is_branch(self) -> bool {
        matches!(
            self,
            Op::Jump
                | Op::JumpIfTrue
                | Op::JumpIfFalse
                | Op::JumpIfNullish
                | Op::Return
                | Op::ReturnValue
                | Op::ReturnUndefined
                | Op::Throw
                | Op::EndFinally
                | Op::Await
                | Op::Yield
        )
    }
}

/// One cold decoded wire/debug instruction.
///
/// Runtime execution never stores this DTO: [`Function::code`] is authoritative
/// [`FunctionCode`]. The byte PC exists only while validating or displaying a
/// serialized byte stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    /// Byte offset within the cold serialized instruction stream.
    pub pc: u32,
    /// Opcode.
    pub op: Op,
    /// Operands in declaration order.
    pub operands: Vec<Operand>,
}

/// One operand value with a kind tag for the JSON dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Operand {
    /// Register index (locals + scratch live in one register window).
    Register(u16),
    /// Index into [`BytecodeModule::constants`].
    ConstIndex(u32),
    /// Inline signed 32-bit immediate (used by `LoadInt32`).
    Imm32(i32),
}

/// One source-span entry attached to a `pc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpanEntry {
    /// Program counter.
    pub pc: u32,
    /// Byte offset range into the original source (`(start, end)`).
    pub span: (u32, u32),
}

/// One compiled function.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Function {
    /// Index into `BytecodeModule::functions`.
    pub id: u32,
    /// Display name; falls back to `<main>` for the script entry.
    pub name: String,
    /// Original source span.
    pub span: (u32, u32),
    /// Number of declared local registers.
    pub locals: u16,
    /// Number of scratch registers above the locals.
    pub scratch: u16,
    /// Number of declared parameters. The first `param_count`
    /// register slots are reserved for parameter binding.
    #[serde(default)]
    pub param_count: u16,
    /// ECMAScript `Function.prototype.length` metadata.
    #[serde(default)]
    pub length: u16,
    /// Number of fresh [`UpvalueCell`]s the prologue allocates for
    /// this function's own locals that are captured by inner
    /// closures. The frame's `upvalues` array is laid out as
    /// `[own_upvalues..., parent_upvalues...]`; own-upvalues live
    /// at indices `0..own_upvalue_count` (stable from compile-time)
    /// and parent-passed captures follow.
    #[serde(default)]
    pub own_upvalue_count: u16,
    /// `true` when this compiled function body executes as strict
    /// ECMAScript code. The compiler sets this from the source type
    /// and directive prologue; runtime call setup reads it for
    /// observable strict/sloppy semantics such as `this` binding and
    /// `arguments` object shape.
    #[serde(default)]
    pub is_strict: bool,
    /// `true` when this record is an arrow function. Arrow bodies
    /// inherit the enclosing function's `this` lexically, so
    /// `MakeClosure` snapshots the current frame's `this` into the
    /// resulting closure value at construction time. Regular
    /// function declarations and expressions have `false` here and
    /// receive `this` from the call site instead.
    #[serde(default)]
    pub is_arrow: bool,
    /// `true` when this record is a MethodDefinition body — a class
    /// or object-literal method / accessor. Methods are not
    /// constructors and §10.2.5 MakeConstructor never runs on them,
    /// so they carry no implicit `prototype` own property.
    #[serde(default)]
    pub is_method: bool,
    /// `true` when this function declares a rest parameter
    /// (`function f(a, b, ...rest) { … }`). The call dispatcher
    /// honours the flag by stashing arguments past `param_count`
    /// onto the new frame's `rest_args` slot for
    /// [`Op::CollectRest`] to materialise.
    #[serde(default)]
    pub has_rest: bool,
    /// `true` when this function was declared with the `async`
    /// keyword. The runtime treats async-call entry specially: it
    /// synthesises a fresh pending [`crate::Constant::FunctionId`]
    /// at the call site (well, the runtime allocates a pending
    /// promise — see `crates/otter-vm/src/lib.rs`'s
    /// `invoke()`), writes that promise into the caller's `dst`
    /// register, and parks the new frame so [`Op::Await`] can
    /// suspend it cleanly. A return / unhandled throw from an
    /// async frame settles its parked promise rather than writing
    /// the value back into the caller's register.
    #[serde(default)]
    pub is_async: bool,
    /// `true` when this function was declared with the `*` marker
    /// (`function*` / generator method / generator class member).
    /// The runtime treats generator-call entry specially: instead
    /// of running the body inline, it allocates a fresh
    /// `Value::Generator` whose paused frame mirrors the function
    /// call at entry. Subsequent `.next(value)` calls resume the
    /// paused frame on a sub-stack until either an
    /// [`Self::Yield`] dispatches (returning `{value, done: false}`)
    /// or the body returns (returning `{value, done: true}`).
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-generator-objects>
    #[serde(default)]
    pub is_generator: bool,
    /// `true` when this function is an `async function*` declaration
    /// — implies both [`Self::is_async`] and [`Self::is_generator`]
    /// for compile-time predicates, but the runtime entry path
    /// keys off this flag to wrap each `.next` / `.return` /
    /// `.throw` call in a Promise per §27.6.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-asyncgenerator-objects>
    #[serde(default)]
    pub is_async_generator: bool,
    /// `true` when this function is the synthesised
    /// `<module-init>` for an ES module fragment. Module-init
    /// functions take two implicit parameters (`module_env`,
    /// `import_meta`) that the linker's `<entry>` driver passes
    /// in; closures defined inside the body capture these via
    /// upvalues.
    ///
    /// The flag is currently informational — the runtime treats
    /// the `<module-init>` body identically to any other call.
    /// It exists so the disassembler / dump can render the role,
    /// and so future slices that want to special-case module
    /// initialisation (e.g. capability gating, top-level await)
    /// have a stable hook.
    #[serde(default)]
    pub is_module: bool,
    /// `true` when this function is the constructor of a *derived*
    /// class (`class C extends B { … }`). Derived constructors start
    /// with `this` in the TDZ: reading `this` (or `super.foo`) before
    /// the `super(...)` call is a `ReferenceError`, and `this` is only
    /// bound once [`Op::BindThisValue`] runs with the `super()`
    /// result. Base-class constructors and ordinary functions leave
    /// this `false` and receive `this` pre-bound at call entry.
    ///
    /// # See also
    /// - <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>
    #[serde(default)]
    pub is_derived_constructor: bool,
    /// `true` when the function body references the `arguments`
    /// identifier and the function is not an arrow (arrows
    /// inherit `arguments` lexically per §10.2.1.4 — the foundation
    /// flags arrows as `false` so their parent frame's
    /// `incoming_args` is consulted via the upvalue chain).
    ///
    /// When set, the call dispatcher stashes the full incoming
    /// argv into the new frame's `incoming_args` so
    /// [`Op::CollectArguments`] can wrap it as an Array.
    #[serde(default)]
    pub needs_arguments: bool,
    /// `true` when the body may read `arguments.callee` (a literal
    /// `.callee` member or any computed `arguments[expr]` access). Gates
    /// the per-call recording of the invoked closure — hot
    /// arguments-using functions that never touch `callee` skip the
    /// cold-frame acquire entirely.
    pub uses_arguments_callee: bool,
    /// Arguments object shape requested by the compiler for this
    /// function. Strict functions, arrows, and non-simple parameter
    /// lists stay unmapped. Sloppy functions with simple parameters
    /// and a body reference to `arguments` may use the mapped form.
    #[serde(default)]
    pub arguments_object_kind: ArgumentsObjectKind,
    /// Indexed argument-to-parameter bindings for the sloppy mapped
    /// arguments object. Entries are in source parameter order after
    /// duplicate-name filtering: duplicate simple parameters keep
    /// only the last matching binding per ECMA-262
    /// CreateMappedArgumentsObject.
    #[serde(default)]
    pub mapped_argument_bindings: Vec<MappedArgumentBinding>,
    /// The source-module URL this function belongs to (e.g.
    /// `"file:///path/to/other.ts"`), recorded by the linker
    /// during module-fragment merging. The runtime threads this
    /// onto each call-frame's `module_url` field so `Op::ImportNamespace`
    /// can resolve specifiers against the correct referrer.
    /// Empty string for non-module functions (e.g. the linker's
    /// synthesised `<entry>` driver) — those frames inherit their
    /// caller's URL or stay empty.
    #[serde(default)]
    pub module_url: String,
    /// §19.2.1.3 EvalDeclarationInstantiation support. Non-empty when
    /// this function body contains a direct `eval(...)` call site: the
    /// compiler promotes every function-scope binding (parameters,
    /// `var` / function declarations, top-level lexicals, the
    /// `arguments` binding) into an own-upvalue cell and records the
    /// name → cell-index mapping here. `Op::Eval` reads the table to
    /// hand the eval body its caller variable environment.
    ///
    /// Also set on a compiled eval `<main>` itself, where it lists the
    /// *new* var-scoped names the eval body declares (cells the caller
    /// frame must adopt so later code observes the bindings).
    #[serde(default)]
    pub direct_eval_bindings: Vec<DirectEvalBinding>,
    /// `true` when this function body (including class field
    /// initializers compiled into a constructor) contains a direct
    /// eval call site. `Op::Eval` uses it as the §19.2.1.1
    /// `inFunction` signal — the binding table above may legitimately
    /// be empty (a synthesized constructor with no own bindings).
    #[serde(default)]
    pub contains_direct_eval: bool,
    /// Verbatim source text of the function / class definition
    /// (§20.2.3.5 [[SourceText]]). Populated for user code by slicing
    /// the original source over `source_text_span` (or `span` when
    /// unset); `None` for synthesized functions with no backing
    /// source, which `toString` renders in the `NativeFunction` form.
    #[serde(default)]
    pub source_text: Option<String>,
    /// Byte range to slice for [[SourceText]] when it differs from the
    /// executable `span`: a class constructor reports the whole
    /// `class … {}` definition, and a method / accessor reports its
    /// `MethodDefinition` (name and `get`/`set`/`*`/`async` prefixes
    /// included). Compile-internal — consumed before the module is
    /// serialized.
    #[serde(skip)]
    pub source_text_span: Option<(u32, u32)>,
    /// Authoritative execution wordcode.
    pub code: FunctionCode,
    /// `pc -> source span` table.
    pub spans: Vec<SpanEntry>,
}

/// One tagged-template call site (§13.2.8.4): cooked strings
/// (`None` for invalid escape sequences, observed as `undefined`)
/// plus the raw spellings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TemplateSite {
    /// Cooked values; `None` = invalid escape (undefined).
    pub cooked: Vec<Option<String>>,
    /// Raw source spellings.
    pub raw: Vec<String>,
}

/// Runtime shape for a materialised `arguments` object.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentsObjectKind {
    /// Strict/unmapped arguments object.
    #[default]
    Unmapped,
    /// Sloppy simple-parameter mapped arguments object.
    Mapped,
}

/// One caller-scope binding a direct `eval` body can see, or one new
/// var-scoped binding an eval body introduces into its caller.
/// `upvalue` indexes the owning frame's upvalue array.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectEvalBinding {
    /// `true` when the cell is a PASSTHROUGH CAPTURE from an
    /// enclosing function rather than the caller's own
    /// variable-environment binding: a direct eval may READ it, but
    /// a `var` of the same name in the eval body declares a fresh
    /// caller-frame binding (§19.2.1.3 — HasVarDeclaration consults
    /// the caller's varEnv only).
    #[serde(default)]
    pub captured: bool,
    /// Source-level binding name.
    pub name: String,
    /// Own-upvalue cell index inside the owning function's frame.
    pub upvalue: u16,
    /// `true` for `let` / `const` / `class` bindings. A sloppy direct
    /// eval whose body var-declares a name that collides with a caller
    /// lexical binding is a runtime `SyntaxError` (§19.2.1.3 step 5).
    pub lexical: bool,
    /// `true` for a `const` / `class` caller binding. An eval body
    /// assigning to it throws `TypeError` in every mode (§13.3.1).
    #[serde(default)]
    pub is_const: bool,
    /// `true` for a named function expression's self-name binding
    /// (§10.2.11 funcEnv). An eval body assigning to it throws
    /// `TypeError` in strict mode and is silently dropped in sloppy
    /// mode (§9.1.1.1.5 SetMutableBinding, immutable binding).
    #[serde(default)]
    pub fn_self_name: bool,
}

/// One argument index aliased to one formal parameter binding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MappedArgumentBinding {
    /// Argument object index, stored as a decimal string property at runtime.
    pub argument_index: u16,
    /// Source formal name, retained for bytecode dumps and audits.
    pub formal_name: String,
    /// Storage backing the parameter binding.
    pub storage: ArgumentBindingStorage,
}

/// Storage location for a mapped formal parameter binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArgumentBindingStorage {
    /// Parameter lives in a frame register.
    Register {
        /// Frame register index.
        reg: u16,
    },
    /// Parameter lives in one of the frame's own upvalue cells.
    Upvalue {
        /// Frame upvalue-cell index.
        idx: u16,
    },
}

/// Source-language flavor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceKind {
    /// JavaScript (`.js`, `.mjs`, `.cjs`).
    JavaScript,
    /// TypeScript (`.ts`, `.mts`, `.cts`).
    TypeScript,
}

/// Constant-pool entry referenced by [`Operand::ConstIndex`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Constant {
    /// String constant. Stored as WTF-16 code units to round-trip
    /// surrogates losslessly through the JSON dump.
    String {
        /// WTF-16 code units.
        utf16: Vec<u16>,
    },
    /// Numeric constant stored as raw IEEE-754 bits to round-trip
    /// `NaN`, `±Infinity`, and `-0.0` losslessly through JSON.
    Number {
        /// `f64::to_bits` representation.
        bits: u64,
    },
    /// Reference to [`BytecodeModule::functions`] — a function
    /// declaration / expression captured at compile time.
    FunctionId {
        /// Index into `BytecodeModule::functions`.
        index: u32,
    },
    /// Decimal digits of a BigInt literal (no `n` suffix). The
    /// compiler validates the literal at intern time, so the
    /// runtime can fall through to a strict-parse path.
    BigInt {
        /// Decimal-digit string (e.g., `"9007199254740993"`,
        /// `"-1"`).
        decimal: String,
    },
    /// Regular-expression literal `/pattern/flags`. The pattern is
    /// stored as WTF-16 code units to round-trip surrogates through
    /// the JSON dump; flags are restricted to the ASCII subset
    /// `"dgimsuvy"`. The runtime compiles the pattern once on first
    /// load and caches the compiled engine.
    RegExp {
        /// WTF-16 code units of the pattern body (between the
        /// slashes, no flags).
        pattern_utf16: Vec<u16>,
        /// ASCII flag string (`"dgimsuvy"` subset). Validated at
        /// compile time so the runtime can rely on a clean parse.
        flags: String,
    },
}

/// Top-level bytecode container produced by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BytecodeModule {
    /// Module specifier (origin path or virtual name).
    pub module: String,
    /// §13.2.8.4 GetTemplateObject — one entry per tagged-template
    /// Parse Node. The runtime caches the frozen template object
    /// per site, so re-evaluating the same call site hands the tag
    /// the SAME object.
    #[serde(default)]
    pub template_sites: Vec<TemplateSite>,
    /// JavaScript or TypeScript.
    pub source_kind: SourceKind,
    /// Function table; index 0 is `<main>`.
    pub functions: Vec<Function>,
    /// Module-wide constant pool.
    #[serde(default)]
    pub constants: Vec<Constant>,
    /// Linker-populated map from `(referrer_module_url,
    /// specifier_text)` → resolved module URL. The runtime's
    /// [`Op::ImportNamespace`] dispatch consults this table by
    /// reading the caller frame's `module_url` and the operand's
    /// specifier constant.
    ///
    /// Stored as a flat list of `(referrer, specifier, target)`
    /// triples for stable JSON-dump shape; the runtime builds an
    /// in-memory hashmap on first use and caches it on the
    /// interpreter side. Empty for script-mode bytecode.
    #[serde(default)]
    pub module_resolutions: Vec<ModuleResolution>,
    /// Linker-populated map from module URL → function ID of that
    /// module's `<module-init>`. The synthesised `<entry>` driver
    /// reads this to call inits in post-order; runtime dynamic
    /// `import("./literal")` reads it to find the namespace's
    /// initialised `module_env` (registry built lazily on first
    /// import). Empty for script-mode bytecode.
    #[serde(default)]
    pub module_inits: Vec<ModuleInit>,
}

/// One linker-resolved import edge: `(referrer module URL,
/// raw specifier text) → target module URL`. Stored as a flat
/// vector inside [`BytecodeModule`] so the JSON dump round-trips
/// cleanly; the runtime constructs an in-memory hashmap from
/// these on first import.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleResolution {
    /// Module URL of the importing source (e.g.
    /// `"file:///path/to/main.ts"`).
    pub referrer: String,
    /// Raw specifier text from the import statement
    /// (`"./other.ts"`).
    pub specifier: String,
    /// Resolved target module URL.
    pub target: String,
    /// `true` when this edge is a `import defer * as ns from "x"`
    /// deferred import. Eager module evaluation does **not** traverse
    /// deferred edges; the target is evaluated only when its deferred
    /// namespace is first accessed (TC39 import defer).
    #[serde(default)]
    pub deferred: bool,
    /// `true` when this edge comes from a literal `import("x")`
    /// expression preloaded into the graph. Like `deferred`, eager
    /// evaluation skips it — but unlike `import defer`, an async
    /// (top-level-await) target is **not** force-evaluated eagerly:
    /// `import()` evaluates its target on call and settles through
    /// the returned promise (§13.3.10).
    #[serde(default)]
    pub dynamic: bool,
    /// `true` when the runtime added this edge after linking purely so the
    /// `<entry>` driver / import-namespace dispatcher can resolve a module's
    /// environment by URL. A synthetic edge is `(referrer, url, url)` — its
    /// specifier equals its target — and is **not** a real `[[RequestedModules]]`
    /// dependency, so evaluation and dependency walks skip it. Real imports
    /// carry `synthetic == false`, including absolute-URL imports (remote
    /// http/https, hosted `otter:` specifiers) whose specifier also equals the
    /// resolved target and so cannot be told apart by shape alone.
    #[serde(default)]
    pub synthetic: bool,
}

/// One module's `<module-init>` entry record: `URL → function ID`.
/// Populated by the linker after merging module fragments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleInit {
    /// Module URL.
    pub url: String,
    /// Function ID of the module's `<module-init>` in the
    /// merged function table.
    pub function_id: u32,
}

impl BytecodeModule {
    /// Convenience accessor for `<main>`.
    #[must_use]
    pub fn main(&self) -> &Function {
        &self.functions[0]
    }
}
