//! Otter foundation bytecode: container, opcode set, encoding, and dumps.
//!
//! This crate is the single source of truth for the new engine's
//! bytecode shape. It is consumed by `otter-compiler` (writers) and
//! `otter-vm` (readers / executors). It does **not** execute anything.
//!
//! # Contents
//! - [`Op`] — canonical opcode enum (`Nop`, `LoadUndefined`, `Return`
//!   for the harness slice; extended slice-by-slice).
//! - [`Instruction`] — decoded form: `(pc, op, operands)`.
//! - [`Function`] — one compiled function: registers, code, spans,
//!   constants index.
//! - [`BytecodeModule`] — top-level container the compiler emits and
//!   the VM consumes.
//! - [`disasm`] — text disassembler per spec
//!   [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   ).
//! - [`dump`] — JSON dump per the same spec
//!   (`otterBytecodeDumpVersion: 1`).
//!
//! # Invariants
//! - Instructions inside [`Function::code`] are sorted by `pc`
//!   ascending; spans inside [`Function::spans`] are sorted by `pc`.
//! - Mnemonics are `SCREAMING_SNAKE_CASE` and match the strings the
//!   disassembler emits.
//!
//! # See also
//! - [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](
//!     ../../../docs/new-engine/specs/bytecode-dump-disasm-trace.md
//!   )
//! - [`docs/new-engine/adr/0003-public-api-and-cli.md`](
//!     ../../../docs/new-engine/adr/0003-public-api-and-cli.md
//!   )

pub mod disasm;
pub mod dump;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// No operation. Used as a placeholder; cost: one dispatch tick.
    Nop,
    /// `r<dst> = undefined`.
    LoadUndefined,
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
    /// (`"gimsuy"` subset). The runtime compiles the regex once at
    /// load time; subsequent loads of the same constant share the
    /// compiled engine via the constant pool slot.
    LoadRegExp,
    /// `r<dst> = JSON.<name>(args...)`. Operands:
    /// `dst, name_const, argc, args...`.
    ///
    /// Variadic, same shape as [`Op::MathCall`]. The compiler
    /// intercepts the literal `JSON.<name>(...)` call shape so the
    /// runtime does not need a real global object yet. Unknown
    /// `<name>` surfaces as `UnknownIntrinsic`.
    JsonCall,
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
    /// [`Op::JsonCall`]. Resolves `<name>` against the Promise
    /// statics dispatcher; unknown names surface as
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
    /// - `String` / `Array` — looks the method up in the matching
    ///   prototype intrinsic table.
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
    /// runtime clones each cell handle into the new closure's
    /// `upvalues: Rc<[UpvalueCell]>`, so writes through one are
    /// visible through all.
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
    /// Variadic call. Operands: `dst, callee, argc, args...`. The
    /// callee must be a function value at this slice. The callee
    /// receives `this = undefined` (foundation default).
    Call,
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

    /// `r<dst> = GetIterator(r<src>)`. Operands:
    /// `Register(dst), Register(src)`. The runtime produces an
    /// internal iterator value over the source: `Array` walks
    /// elements, `String` walks code units, anything else raises
    /// `TypeMismatch`. Real `[@@iterator]` resolution lands when
    /// `Symbol` arrives (task 37).
    GetIterator,
    /// Drive an iterator one step. Operands:
    /// `Register(value_dst), Register(done_dst), Register(iter)`.
    /// Writes the next value into `value_dst` and a `Boolean` into
    /// `done_dst`; once `done_dst` is `true` the value is
    /// `undefined` and further calls keep returning `done = true`.
    IteratorNext,
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
    /// Variadic call against the `Math` namespace function table.
    /// Operands: `Register(dst), ConstIndex(name), ConstIndex(argc),
    /// Register(arg0), …`. The runtime resolves `name` against the
    /// `math` module's registry; unknown names raise
    /// `UnknownIntrinsic`.
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
    /// `r<obj>.<name> = r<src>`. Operands: `obj, name_const, src`.
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
    /// Wrap the value in `r<src>` as a fulfilled `Promise` and
    /// write to `r<dst>`. Operands:
    /// `Register(dst), Register(src)`.
    ///
    /// Used by the literal-string dynamic-import lowering: after
    /// [`Op::ImportNamespace`] resolves the namespace synchronously
    /// the compiler wraps the result in a pre-fulfilled promise so
    /// the surface matches `import("./x").then(ns => ...)`.
    PromiseFulfilledOf,

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
}

impl Op {
    /// Canonical mnemonic spelling for disassembly and trace events.
    #[must_use]
    pub const fn mnemonic(self) -> &'static str {
        match self {
            Op::Nop => "NOP",
            Op::LoadUndefined => "LOAD_UNDEFINED",
            Op::Return => "RETURN",
            Op::LoadString => "LOAD_STRING",
            Op::LoadNumber => "LOAD_NUMBER",
            Op::LoadInt32 => "LOAD_INT32",
            Op::LoadBigInt => "LOAD_BIGINT",
            Op::LoadRegExp => "LOAD_REGEXP",
            Op::JsonCall => "JSON_CALL",
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
            Op::Throw => "THROW",
            Op::EnterTry => "ENTER_TRY",
            Op::LeaveTry => "LEAVE_TRY",
            Op::EndFinally => "END_FINALLY",
            Op::NewError => "NEW_ERROR",
            Op::GetIterator => "GET_ITERATOR",
            Op::IteratorNext => "ITERATOR_NEXT",
            Op::ArrayPush => "ARRAY_PUSH",
            Op::CallSpread => "CALL_SPREAD",
            Op::New => "NEW",
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
            Op::Call => "CALL",
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
            Op::PromiseFulfilledOf => "PROMISE_FULFILLED_OF",
            Op::Await => "AWAIT",
        }
    }

    /// Declared operand arity. `CallMethodValue` is variadic; the
    /// instruction stream stores `dst, recv, name_const, argc`
    /// followed by `argc` register operands, so the actual operand
    /// count is `4 + argc`. `operand_count` returns the **prefix**
    /// length; consumers walk the variadic tail by reading `argc`.
    /// `CallWithThis` and `BindFunction` follow the same convention
    /// with an extra `this` register before `argc`.
    #[must_use]
    pub const fn operand_count(self) -> usize {
        match self {
            Op::Nop | Op::ReturnUndefined | Op::LeaveTry | Op::EndFinally => 0,
            Op::LoadUndefined
            | Op::LoadNull
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadThis
            | Op::Return
            | Op::ReturnValue
            | Op::Jump
            | Op::TdzError
            | Op::Throw
            | Op::NewObject
            | Op::CollectRest => 1,
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
            | Op::MakeFunction
            | Op::MathLoad
            | Op::Await
            | Op::ImportNamespace
            | Op::PromiseFulfilledOf => 2,
            Op::GetStringIndex
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
            | Op::StoreProperty
            | Op::DeleteProperty
            | Op::Instanceof => 3,
            Op::GetPrototype
            | Op::SetPrototype
            | Op::ArrayLength
            | Op::NewError
            | Op::GetIterator
            | Op::ArrayPush => 2,
            Op::IteratorNext => 3,
            Op::CallSpread => 4,
            // `NewArray` is variadic: `dst, count, elems...`. The
            // dispatcher reads the count and walks the trailing
            // operands.
            Op::NewArray => 2,
            Op::LoadElement | Op::StoreElement => 3,
            Op::CallMethodValue => 4, // dst, recv, name_const, argc
            Op::MathCall => 3,        // dst, name_const, argc — args follow
            Op::JsonCall => 3,        // dst, name_const, argc — args follow
            Op::QueueMicrotask => 2,  // callee, argc — args follow
            Op::PromiseNew => 3,      // dst, executor_reg, scratch_dst
            Op::PromiseCall => 3,     // dst, name_const, argc — args follow
            Op::Call | Op::New => 3,  // dst, callee, argc — args follow
            Op::MakeClass => 4,       // dst, ctor, prototype, statics
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
        )
    }
}

/// One decoded instruction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    /// Program counter (byte offset within the function's `code`).
    pub pc: u32,
    /// Opcode.
    pub op: Op,
    /// Operands in declaration order.
    pub operands: Vec<Operand>,
}

/// One operand value with a kind tag for the JSON dump.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Number of fresh [`UpvalueCell`]s the prologue allocates for
    /// this function's own locals that are captured by inner
    /// closures. The frame's `upvalues` array is laid out as
    /// `[own_upvalues..., parent_upvalues...]`; own-upvalues live
    /// at indices `0..own_upvalue_count` (stable from compile-time)
    /// and parent-passed captures follow.
    #[serde(default)]
    pub own_upvalue_count: u16,
    /// `true` when this record is an arrow function. Arrow bodies
    /// inherit the enclosing function's `this` lexically, so
    /// `MakeClosure` snapshots the current frame's `this` into the
    /// resulting closure value at construction time. Regular
    /// function declarations and expressions have `false` here and
    /// receive `this` from the call site instead.
    #[serde(default)]
    pub is_arrow: bool,
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
    /// promise — see `crates-next/otter-vm/src/lib.rs`'s
    /// `invoke()`), writes that promise into the caller's `dst`
    /// register, and parks the new frame so [`Op::Await`] can
    /// suspend it cleanly. A return / unhandled throw from an
    /// async frame settles its parked promise rather than writing
    /// the value back into the caller's register.
    #[serde(default)]
    pub is_async: bool,
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
    /// Encoded instructions.
    pub code: Vec<Instruction>,
    /// `pc -> source span` table.
    pub spans: Vec<SpanEntry>,
}

/// Source-language flavor (per ADR-0002).
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
    /// `"gimsuy"`. The runtime compiles the pattern once on first
    /// load and caches the compiled engine.
    RegExp {
        /// WTF-16 code units of the pattern body (between the
        /// slashes, no flags).
        pattern_utf16: Vec<u16>,
        /// ASCII flag string (`"gimsuy"` subset). Validated at
        /// compile time so the runtime can rely on a clean parse.
        flags: String,
    },
}

/// Top-level bytecode container produced by the compiler.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BytecodeModule {
    /// Module specifier (origin path or virtual name).
    pub module: String,
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
