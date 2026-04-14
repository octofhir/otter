# Otter Bytecode v2 — V8 Ignition-Style Accumulator ISA

> **Status**: Phase 0 design document. This is the source of truth for Phase 1+ implementation. Changes go here first, then into code.
>
> **Supersedes**: `crates/otter-vm/src/bytecode.rs` (v1, 3-address register ISA, 121 opcodes).

## 1. Goals

1. **Eliminate temp explosion.** Every intermediate expression value lives in an implicit accumulator, not a fresh frame slot. `s = (s + i) | 0` lowers to 4 instructions (`Ldar s; Add i; BitwiseOr 0; Star s`) instead of today's 6+ with three live temp slots.
2. **Shrink bytecode by 40–50%.** Variable-width operand encoding (1-byte default, `Wide`/`ExtraWide` prefix for 16- and 32-bit operands) replaces the current 64-bit-per-instruction fixed format.
3. **Simplify the baseline JIT.** The accumulator pins to a single callee-saved CPU register (`x21` on aarch64) across every compiled function — no per-op register allocation, no `box_int32 + str ; ldr + extract` round-trip between arithmetic ops.
4. **Unblock speculative guard elision.** Every feedback-carrying op (arithmetic, comparison, property, call) gets a `FeedbackSlotId` allocated at compile time, indexed by bytecode-offset, so the JIT's already-wired `trust_int32` path actually fires.
5. **Stay idiomatic-ECMAScript.** No JS semantics change. Test262 pass rate must not regress at any phase.

## 2. Non-goals (Phase 0–5)

- On-disk bytecode caching (JSC 2019 metadata-sidecar design). Keep metadata in-process for now; Phase F polish.
- Register file restructuring. `FrameLayout` keeps its hidden / parameter / local / temp ranges.
- New object model or shape semantics.
- New JS feature work — this is a pure ISA migration.

## 3. Execution model

### 3.1 Accumulator

A single 64-bit `RegisterValue` slot, live for the lifetime of one `Activation`. Never named in an operand.

- **Reads**: `Lda*` instructions; arithmetic/comparison/test/jump-if ops read it as the (implicit) left operand.
- **Writes**: almost every non-store op writes it (arith, compare, load, property-get, call return, iterator-next, etc.).
- **Stores to named register**: explicit `Star r` op.

The accumulator is part of the function's abstract state. On suspend (`Yield`, `Await`), it must be saved into the generator/async continuation. On exception unwind and JIT deopt, it must be materialized into the interpreter's frame.

### 3.2 Named register file

Unchanged from v1: `FrameLayout { hidden_count, parameter_count, local_count, temporary_count }`. Bytecode v2 addresses registers through `BytecodeRegister(u16)` with `resolve_user_visible` translating to absolute slots (identical helper used today). The accumulator is separate; register `r0` is not the accumulator.

In practice, v2 compilation uses dramatically fewer temp slots than v1 (accumulator absorbs most intermediates), so `temporary_count` in emitted frames shrinks — but `FrameLayout` itself keeps the same shape.

### 3.3 Feedback vector

Every bytecode instruction that requires runtime specialization receives a `FeedbackSlotId` at compile time. Slot allocation is driven by the compiler (not inline in the bytecode stream), populated as a `FeedbackTableLayout` attached to `Function`. At dispatch time, the instruction's implicit feedback slot is derived from its byte offset via a compile-time map.

Instruction families that receive a feedback slot:

- Arithmetic binary (`Add r`, `Sub r`, …, `BitwiseOr r`, …) — `FeedbackKind::Arithmetic`.
- Arithmetic unary (`Inc`, `Dec`, `Neg`, `BitwiseNot`) — `Arithmetic`.
- Comparison (`TestEq r`, `TestLt r`, …) — `Comparison`.
- Property (`LdaNamedProperty r k`, `StaNamedProperty r k`, `LdaKeyedProperty r`, …) — `Property`.
- Call (`CallProperty`, `CallAnyReceiver`, `Construct`) — `Call`.
- Branch (`JumpIfTrue`, `JumpIfFalse`, …) — `Branch`.

This is the critical fix that activates the dormant `trust_int32` path in `analyze_template_candidate_with_feedback`.

## 4. Instruction encoding

### 4.1 Byte-stream format

- A function's `Bytecode` is a `Box<[u8]>` plus a parallel `Box<[u16]>` of PC→feedback-slot indices (sparse; `u16::MAX` means "no feedback slot").
- Dispatch iterates a PC cursor over the byte stream. Each instruction starts at a byte offset and consumes 1..=N bytes.
- Instructions are **not** 4-byte aligned. Alignment is per-byte.

### 4.2 Operand encoding

Each opcode has a fixed operand *arity* and operand *kinds* but variable operand *width*. The operand width of the whole instruction is set by an optional prefix byte:

- No prefix ⇒ all operands are 1 byte.
- `Wide` (0xFE) prefix ⇒ all operands are 2 bytes (little-endian).
- `ExtraWide` (0xFF) prefix ⇒ all operands are 4 bytes (little-endian).

This matches V8 Ignition exactly (per-instruction promotion, not per-operand). Every prefix applies to the immediately-following opcode.

Operand kinds:

| Kind | Description |
|---|---|
| `Reg` | `BytecodeRegister` index into the current frame's register file. Unsigned. |
| `Imm` | Signed small integer literal (i8/i16/i32 depending on operand width). |
| `Idx` | Index into a side table: constant pool (`ConstantId`), string pool (`StringId`), property-name pool (`PropertyNameId`), closure template (`ClosureId`), RegExp template (`RegExpId`), BigInt pool (`BigIntId`), float pool (`FloatId`), upvalue (`UpvalueId`), class id. Unsigned. |
| `JumpOff` | Signed byte offset from the instruction *after* the jump (Ignition convention). |
| `RegList` | Pair `(base: Reg, count: Reg-width-unsigned)` describing a contiguous outgoing register window. Used by Call ops. |

### 4.3 Common instruction size

| Encoding | Bytes |
|---|---|
| `Ldar r` (1 `Reg` operand) | 2 |
| `Add r` | 2 |
| `LdaSmi imm8` | 2 |
| `LdaSmi imm16` (Wide) | 4 (`Wide` + opcode + 2B imm) |
| `LdaConst k` | 2 |
| `CallProperty r_target r_recv RegList` | 4 |
| `JumpIfFalse off8` | 2 |
| `JumpIfFalse off16` (Wide) | 4 |
| `LdaGlobal Idx` | 2 |
| `Return` | 1 |

Expected average: ≈ 2.4 bytes/instruction, vs v1's fixed 8 bytes. Matches the Ignition ≈ 2.5 B/insn reported by V8.

### 4.4 Feedback slot mapping

Each instruction that needs a feedback slot records its slot id in a parallel `bytecode_pc → FeedbackSlotId(u16)` table, emitted at compile time. Sparse: instructions without feedback have no entry. The table is part of the `Function` side metadata, not the byte stream.

At dispatch the interpreter does *not* read this table (it's a pure JIT/feedback side channel), so instruction decoding stays tight.

## 5. Opcode families

Below is the complete v2 opcode set, organized by family. Each row lists the opcode, its operand shape, acc effect, feedback-slot kind (if any), and the v1 opcode(s) it replaces.

Operand shape notation: `op kind1 kind2 …` — e.g., `Ldar Reg` means one `Reg` operand; `CallProperty Reg Reg RegList` means two `Reg`s and a `RegList`.

### 5.1 Accumulator load/store

| v2 op | Operands | Acc | Feedback | Replaces v1 |
|---|---|---|---|---|
| `Ldar` | `Reg` | = r | — | — (new explicit load) |
| `Star` | `Reg` | written back to r | — | — (new explicit store) |
| `Mov` | `Reg`, `Reg` | unchanged | — | `Move` |
| `LdaSmi` | `Imm` | = imm (int32) | — | `LoadI32` |
| `LdaUndefined` | — | = undefined | — | `LoadUndefined` |
| `LdaNull` | — | = null | — | `LoadNull` |
| `LdaTrue` | — | = true | — | `LoadTrue` |
| `LdaFalse` | — | = false | — | `LoadFalse` |
| `LdaTheHole` | — | = `TAG_HOLE` | — | `LoadHole` |
| `LdaNaN` | — | = NaN | — | `LoadNaN` |
| `LdaConstF64` | `Idx` | = float_pool[idx] | — | `LoadF64` |
| `LdaConstStr` | `Idx` | = string_pool[idx] | — | `LoadString` |
| `LdaConstBigInt` | `Idx` | = bigint_pool[idx] | — | `LoadBigInt` |
| `LdaException` | — | = pending exception | — | `LoadException` |
| `LdaNewTarget` | — | = new.target | — | `LoadNewTarget` |
| `LdaCurrentClosure` | — | = current closure | — | `LoadCurrentClosure` |
| `LdaThis` | — | = `this` | — | `LoadThis` |

### 5.2 Arithmetic (accumulator-based)

All binary arithmetic ops read acc as the lhs, the named register as rhs, and write acc. Feedback slot kind is `Arithmetic`.

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `Add` | `Reg` | acc = acc + r | `Add` |
| `Sub` | `Reg` | acc = acc − r | `Sub` |
| `Mul` | `Reg` | acc = acc × r | `Mul` |
| `Div` | `Reg` | acc = acc ÷ r | `Div` |
| `Mod` | `Reg` | acc = acc mod r | `Mod` |
| `Exp` | `Reg` | acc = acc ** r | `Exp` |
| `BitwiseAnd` | `Reg` | acc = acc & r | `BitAnd` |
| `BitwiseOr` | `Reg` | acc = acc \| r | `BitOr` |
| `BitwiseXor` | `Reg` | acc = acc ^ r | `BitXor` |
| `Shl` | `Reg` | acc = acc << r | `Shl` |
| `Shr` | `Reg` | acc = acc >> r (signed) | `Shr` |
| `UShr` | `Reg` | acc = acc >>> r (unsigned) | `UShr` |

**Smi-immediate variants** (small wins on hot loops; Ignition uses them heavily). Encoded with an `Imm` operand instead of `Reg`. Feedback slot still kind `Arithmetic`.

| v2 op | Operands | Effect | Replaces v1 pattern |
|---|---|---|---|
| `AddSmi` | `Imm` | acc = acc + imm | `LoadI32 tmp, imm; Add ...` |
| `SubSmi` | `Imm` | acc = acc − imm | same |
| `MulSmi` | `Imm` | acc = acc × imm | same |
| `BitwiseOrSmi` | `Imm` | acc = acc \| imm | same |
| `BitwiseAndSmi` | `Imm` | acc = acc & imm | same |
| `ShlSmi` | `Imm` | acc = acc << imm | same |
| `ShrSmi` | `Imm` | acc = acc >> imm (signed) | same |

**Unary arithmetic**:

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `Inc` | — | acc = acc + 1 | — (was `LoadI32 + Add`) |
| `Dec` | — | acc = acc − 1 | — |
| `Negate` | — | acc = −acc | — |
| `BitwiseNot` | — | acc = ~acc | — |
| `LogicalNot` | — | acc = !acc | `Not` |
| `TypeOf` | — | acc = typeof acc | `TypeOf` |
| `ToBoolean` | — | acc = ToBoolean(acc) | — |
| `ToNumber` | — | acc = ToNumber(acc) | `ToNumber` |
| `ToString` | — | acc = ToString(acc) | `ToString` |
| `ToPropertyKey` | — | acc = ToPropertyKey(acc) | `ToPropertyKey` |

### 5.3 Comparisons

All binary comparisons: acc = TestOp(acc, r). Feedback slot kind `Comparison`.

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `TestEqual` | `Reg` | acc = (acc == r) | `LooseEq` |
| `TestEqualStrict` | `Reg` | acc = (acc === r) | `Eq` |
| `TestLessThan` | `Reg` | acc = (acc < r) | `Lt` |
| `TestGreaterThan` | `Reg` | acc = (acc > r) | `Gt` |
| `TestLessThanOrEqual` | `Reg` | acc = (acc ≤ r) | `Lte` |
| `TestGreaterThanOrEqual` | `Reg` | acc = (acc ≥ r) | `Gte` |
| `TestInstanceOf` | `Reg` | acc = (acc instanceof r) | `InstanceOf` |
| `TestIn` | `Reg` | acc = (acc in r) | `HasProperty` |
| `TestNull` | — | acc = (acc === null) | — |
| `TestUndefined` | — | acc = (acc === undefined) | — |
| `TestUndetectable` | — | acc = IsUndetectable(acc) | — |
| `TestTypeOf` | `Imm` (type tag) | acc = (typeof acc === tag) | — |
| `InPrivate` | `Reg`, `Idx` (private name) | acc = (#name in r) | `InPrivate` |

### 5.4 Jumps (read acc directly for the conditional forms)

| v2 op | Operands | Semantics | Replaces v1 |
|---|---|---|---|
| `Jump` | `JumpOff` | unconditional | `Jump` |
| `JumpIfTrue` | `JumpOff` | if acc is truthy | `JumpIfTrue` (conditional on reg — now conditional on acc) |
| `JumpIfFalse` | `JumpOff` | if acc is falsy | `JumpIfFalse` |
| `JumpIfNull` | `JumpOff` | if acc === null | — |
| `JumpIfNotNull` | `JumpOff` | if acc !== null | — |
| `JumpIfUndefined` | `JumpOff` | if acc === undefined | — |
| `JumpIfNotUndefined` | `JumpOff` | if acc !== undefined | — |
| `JumpIfJSReceiver` | `JumpOff` | if acc is an object (not primitive) | — |
| `JumpIfToBooleanTrue` | `JumpOff` | if ToBoolean(acc) | — |
| `JumpIfToBooleanFalse` | `JumpOff` | if !ToBoolean(acc) | — |

### 5.5 Property access

Named property: key comes from a `PropertyNameId` in the `Idx` operand. Feedback slot kind `Property`.

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `LdaNamedProperty` | `Reg`, `Idx` | acc = r[name] | `GetProperty` |
| `StaNamedProperty` | `Reg`, `Idx` | r[name] = acc (returns acc) | `SetProperty` |
| `LdaKeyedProperty` | `Reg` | acc = r[acc] (key in acc) | `GetIndex` |
| `StaKeyedProperty` | `Reg`, `Reg` | r0[r1] = acc | `SetIndex` |
| `DelNamedProperty` | `Reg`, `Idx` | acc = delete r[name] | `DeleteProperty` |
| `DelKeyedProperty` | `Reg` | acc = delete r[acc] | `DeleteComputed` |
| `LdaGlobal` | `Idx` | acc = globalThis[name] | `GetGlobal` |
| `StaGlobal` | `Idx` | globalThis[name] = acc | `SetGlobal` |
| `StaGlobalStrict` | `Idx` | strict-mode global store | `SetGlobalStrict` |
| `TypeOfGlobal` | `Idx` | acc = typeof globalThis[name] | `TypeOfGlobal` |
| `LdaUpvalue` | `Idx` | acc = upvalues[idx] | `GetUpvalue` |
| `StaUpvalue` | `Idx` | upvalues[idx] = acc | `SetUpvalue` |

### 5.6 Calls

All call ops place arguments in a contiguous register window `[base, base+count)`. The receiver and callee are named registers. Return value goes into acc. Feedback slot kind `Call`.

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `CallAnyReceiver` | `Reg` callee, `Reg` recv, `RegList` args | acc = callee.call(recv, args…) | `CallClosure` |
| `CallProperty` | `Reg` target, `Idx` name, `RegList` args | acc = target[name](target, args…) | — (bespoke optimization) |
| `CallUndefinedReceiver` | `Reg` callee, `RegList` args | acc = callee(undef, args…) | — |
| `CallDirect` | `Idx` func, `RegList` args | acc = module.fn\[func\](args…) | `CallDirect` |
| `CallSpread` | `Reg` callee, `Reg` recv, `RegList` args (last is spread) | acc = callee.call(recv, …args) | `CallSpread` |
| `Construct` | `Reg` callee, `Reg` newtarget, `RegList` args | acc = new callee(args…) | — |
| `ConstructSpread` | `Reg` callee, `Reg` newtarget, `RegList` args (last spread) | spreaded construct | — |
| `CallEval` | `Reg` callee, `Reg` recv, `RegList` args | direct `eval(…)` path | `CallEval` |
| `CallSuper` | `RegList` args | super(args…) | `CallSuper` / `CallSuperForward` |
| `CallSuperSpread` | `RegList` args (last spread) | super(…args) | `CallSuperSpread` |
| `TailCall` | `Reg` callee, `Reg` recv, `RegList` args | tail-position call (reuses frame) | `TailCallClosure` |

### 5.7 Control flow

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `Return` | — | return acc | `Return` |
| `Throw` | — | throw acc | `Throw` |
| `ReThrow` | — | rethrow pending exception | — |
| `Nop` | — | — | `Nop` |
| `Abort` | `Imm` reason | panic/ICE | — |

### 5.8 Generators / async

Acc carries the value in/out of `Yield`/`Resume`/`Await`. Suspend sites save acc into the generator state alongside named registers.

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `Yield` | — | suspend, value = acc | `Yield` |
| `YieldStar` | `Reg` (iterator) | delegate yield | `YieldStar` |
| `SuspendGenerator` | — | framework-internal suspend | — |
| `Resume` | `Reg` (gen state) | restore frame, acc = resumed value | — |
| `Await` | — | await acc | `Await` |

### 5.9 Iteration

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `GetIterator` | `Reg` | acc = iterator of r | `GetIterator` |
| `GetAsyncIterator` | `Reg` | acc = async iterator of r | `GetAsyncIterator` |
| `IteratorNext` | `Reg` | acc = iter.next(); secondary flag in frame slot | `IteratorNext` |
| `IteratorClose` | `Reg` | close iter | `IteratorClose` |
| `ForInEnumerate` | `Reg` | acc = for-in enumerator of r | `GetPropertyIterator` |
| `ForInNext` | `Reg` (enumerator), `Reg` (state) | acc = next key or undef | `PropertyIteratorNext` |
| `SpreadIntoArray` | `Reg` (array) | append spread of acc into r | `SpreadIntoArray` |
| `ArrayPush` | `Reg` (array) | r.push(acc) | `ArrayPush` |
| `CreateEnumerableOwnKeys` | `Reg` | acc = own enumerable keys of r | `CreateEnumerableOwnKeys` |
| `AssertNotHole` | — | throw TDZ if acc is hole | `AssertNotHole` |
| `AssertConstructor` | — | throw if acc is not constructor | `AssertConstructor` |

### 5.10 Object / array allocation

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `CreateObject` | — | acc = new plain object | `NewObject` |
| `CreateArray` | — | acc = new [] | `NewArray` |
| `CreateClosure` | `Idx` (closure template), `Imm` flags | acc = new closure | `NewClosure` |
| `CreateArguments` | `Imm` kind (mapped / unmapped / rest) | acc = arguments object | `CreateArguments` |
| `CreateRestParameters` | — | acc = rest array | `CreateRestParameters` |
| `CreateRegExp` | `Idx` template | acc = new RegExp | `NewRegExp` |
| `CopyDataProperties` | `Reg` (source) | spread copy props of acc from r | `CopyDataProperties` |
| `CopyDataPropertiesExcept` | `Reg` src, `RegList` excluded | copy minus excluded | `CopyDataPropertiesExcept` |
| `DefineNamedGetter` | `Reg` obj, `Idx` name | obj[name] getter = acc | `DefineNamedGetter` |
| `DefineNamedSetter` | `Reg` obj, `Idx` name | obj[name] setter = acc | `DefineNamedSetter` |
| `DefineComputedGetter` | `Reg` obj, `Reg` key | obj[key] getter = acc | `DefineComputedGetter` |
| `DefineComputedSetter` | `Reg` obj, `Reg` key | obj[key] setter = acc | `DefineComputedSetter` |

### 5.11 Classes / private / super

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `DefineField` | `Reg` obj, `Idx` name | field init | `DefineField` |
| `DefineComputedField` | `Reg` obj, `Reg` key | computed field | `DefineComputedField` |
| `RunClassFieldInitializer` | `Reg` initializer | run initializer | `RunClassFieldInitializer` |
| `SetClassFieldInitializer` | `Reg` | set initializer | `SetClassFieldInitializer` |
| `AllocClassId` | — | acc = new class id (u64) | `AllocClassId` |
| `CopyClassId` | `Reg` src | copy id | `CopyClassId` |
| `DefinePrivateField` | `Reg` obj, `Idx` name | obj.#name = acc (init) | `DefinePrivateField` |
| `GetPrivateField` | `Reg` obj, `Idx` name | acc = obj.#name | `GetPrivateField` |
| `SetPrivateField` | `Reg` obj, `Idx` name | obj.#name = acc | `SetPrivateField` |
| `DefinePrivateMethod` | `Reg` obj, `Idx` name | obj.#m = acc | `DefinePrivateMethod` |
| `DefinePrivateGetter` | `Reg` obj, `Idx` name | private getter = acc | `DefinePrivateGetter` |
| `DefinePrivateSetter` | `Reg` obj, `Idx` name | private setter = acc | `DefinePrivateSetter` |
| `PushPrivateMethod` | `Reg` obj, `Idx` name | | `PushPrivateMethod` |
| `PushPrivateGetter` | `Reg` obj, `Idx` name | | `PushPrivateGetter` |
| `PushPrivateSetter` | `Reg` obj, `Idx` name | | `PushPrivateSetter` |
| `DefineClassMethod` | `Reg` ctor, `Idx` name | method on prototype | `DefineClassMethod` |
| `DefineClassMethodComputed` | `Reg` ctor, `Reg` key | | `DefineClassMethodComputed` |
| `DefineClassGetter` | `Reg` ctor, `Idx` name | | `DefineClassGetter` |
| `DefineClassSetter` | `Reg` ctor, `Idx` name | | `DefineClassSetter` |
| `DefineClassGetterComputed` | `Reg` ctor, `Reg` key | | `DefineClassGetterComputed` |
| `DefineClassSetterComputed` | `Reg` ctor, `Reg` key | | `DefineClassSetterComputed` |
| `SetHomeObject` | `Reg` method, `Reg` home | | `SetHomeObject` |
| `GetSuperProperty` | `Reg` this, `Idx` name | acc = super[name] | `GetSuperProperty` |
| `GetSuperPropertyComputed` | `Reg` this, `Reg` key | acc = super[key] | `GetSuperPropertyComputed` |
| `SetSuperProperty` | `Reg` this, `Idx` name | super[name] = acc | `SetSuperProperty` |
| `SetSuperPropertyComputed` | `Reg` this, `Reg` key | super[key] = acc | `SetSuperPropertyComputed` |
| `ThrowConstAssign` | — | TypeError | `ThrowConstAssign` |

### 5.12 Modules

| v2 op | Operands | Effect | Replaces v1 |
|---|---|---|---|
| `DynamicImport` | `Reg` specifier | acc = import(specifier) | `DynamicImport` |
| `ImportMeta` | — | acc = import.meta | `ImportMeta` |

### 5.13 Prefixes

| v2 op | Operands | Effect |
|---|---|---|
| `Wide` (0xFE) | followed by an opcode and its operands | operands of next insn are 2 bytes |
| `ExtraWide` (0xFF) | followed by an opcode and its operands | operands of next insn are 4 bytes |

## 6. Canonical example: `s = (s + i) | 0`

Assume `s` is frame register r2 and `i` is r3.

### 6.1 v1 (current 3-address) — 6 instructions, 8 bytes each = 48 bytes

```
LoadI32      r4, 0          // tmp4 = 0 (|0 operand prep, only if compiler doesn't inline)
Add          r5, r2, r3     // tmp5 = s + i
LoadI32      r6, 0          // tmp6 = 0
BitOr        r7, r5, r6     // tmp7 = tmp5 | 0
Move         r2, r7         // s = tmp7
... (possibly more for ToNumber around i++)
```

### 6.2 v2 (Ignition-style) — 4 instructions, ~9 bytes

```
Ldar         r2             // acc = s            (2 bytes)
Add          r3             // acc = acc + i       (2 bytes, feedback slot allocated)
BitwiseOrSmi 0              // acc = acc | 0       (2 bytes, feedback slot)
Star         r2             // s = acc             (2 bytes)
```

Four instructions, zero temp slots. In the baseline JIT with accumulator pinned to `x21`:

```
ldr   x21, [x9, r2_off]              ; Ldar
ldr   x10, [x9, r3_off]              ; Add load rhs
check_int32_tag_fast x10, x20
b.ne  <deopt>
sxtw  x10, w10
add   x21, x21, x10                  ; Add (stays in x21)
orr   x21, x21, xzr                  ; BitwiseOrSmi 0 — no-op, acc unchanged if already i32
box_int32 x10, x21
str   x10, [x9, r2_off]              ; Star
```

Compare to today's ≈100-insn loop body. Expected steady-state: ~15× fewer asm instructions per iteration.

## 7. v1 → v2 opcode mapping (complete)

Every v1 opcode has a deterministic lowering to v2 by the v2 compiler. Since v1 opcodes disappear in Phase 6, this table serves as (a) the spec for the v2 source compiler and (b) the cross-check for any remaining v1→v2 translation layer during the migration.

| v1 opcode | v2 replacement |
|---|---|
| `Nop` | `Nop` |
| `Move a, b` | `Ldar b; Star a` (or `Mov b, a` when acc live) |
| `LoadI32 a, imm` | `LdaSmi imm; Star a` |
| `LoadTrue a` / `LoadFalse a` | `LdaTrue / LdaFalse; Star a` |
| `LoadNaN a` | `LdaNaN; Star a` |
| `LoadUndefined a` / `LoadNull a` / `LoadHole a` | `LdaUndefined / LdaNull / LdaTheHole; Star a` |
| `LoadString a, idx` / `LoadF64 a, idx` / `LoadBigInt a, idx` | `LdaConstStr idx / LdaConstF64 idx / LdaConstBigInt idx; Star a` |
| `LoadException a` | `LdaException; Star a` |
| `LoadCurrentClosure a` | `LdaCurrentClosure; Star a` |
| `LoadThis a` | `LdaThis; Star a` |
| `LoadNewTarget a` | `LdaNewTarget; Star a` |
| `Not a, b` | `Ldar b; LogicalNot; Star a` |
| `TypeOf a, b` | `Ldar b; TypeOf; Star a` |
| `Add a, b, c` | `Ldar b; Add c; Star a` |
| `Sub / Mul / Div / Mod / Exp` | same pattern |
| `BitAnd / BitOr / BitXor / Shl / Shr / UShr` | same |
| `Lt / Gt / Gte / Lte / Eq / LooseEq` | `Ldar b; TestLessThan c; Star a` (etc.) |
| `InstanceOf a, b, c` | `Ldar b; TestInstanceOf c; Star a` |
| `HasProperty a, b, c` | `Ldar b; TestIn c; Star a` |
| `InPrivate a, b, idx` | `Ldar b; InPrivate b idx; Star a` — TBD during Phase 2 |
| `GetProperty a, b, idx` | `LdaNamedProperty b idx; Star a` |
| `SetProperty a, b, idx` | `Ldar b; StaNamedProperty a idx` (note: v1's A/B are reversed in store form; Phase 2 pins this down) |
| `GetIndex a, b, c` | `Ldar c; LdaKeyedProperty b; Star a` |
| `SetIndex a, b, c` | `Ldar c; StaKeyedProperty a b` |
| `DeleteProperty a, b, idx` | `DelNamedProperty b idx; Star a` |
| `DeleteComputed a, b, c` | `Ldar c; DelKeyedProperty b; Star a` |
| `GetUpvalue a, idx` | `LdaUpvalue idx; Star a` |
| `SetUpvalue a, idx` | `Ldar a; StaUpvalue idx` |
| `GetGlobal a, idx` | `LdaGlobal idx; Star a` |
| `SetGlobal a, idx` | `Ldar a; StaGlobal idx` |
| `SetGlobalStrict a, idx` | `Ldar a; StaGlobalStrict idx` |
| `TypeOfGlobal a, idx` | `TypeOfGlobal idx; Star a` |
| `Jump off` | `Jump off` |
| `JumpIfTrue a, off` | `Ldar a; JumpIfTrue off` |
| `JumpIfFalse a, off` | `Ldar a; JumpIfFalse off` |
| `Return a` | `Ldar a; Return` |
| `Throw a` | `Ldar a; Throw` |
| `CallDirect a, b, idx` | `CallDirect idx RegList(b,count); Star a` |
| `CallClosure a, b, c` | `CallAnyReceiver b recv RegList(...); Star a` — Phase 2 decides receiver encoding |
| `CallSpread / CallSuper / CallSuperForward / CallSuperSpread` | `CallSpread / CallSuper / CallSuperSpread` |
| `CallEval` | `CallEval` |
| `TailCallClosure` | `TailCall` |
| `NewObject a` / `NewArray a` | `CreateObject / CreateArray; Star a` |
| `NewClosure a, idx` | `CreateClosure idx flags; Star a` |
| `NewRegExp a, idx` | `CreateRegExp idx; Star a` |
| `CreateArguments a, kind` | `CreateArguments kind; Star a` |
| `CreateRestParameters a` | `CreateRestParameters; Star a` |
| `CreateEnumerableOwnKeys a, b` | `Ldar b; CreateEnumerableOwnKeys b; Star a` |
| `GetIterator / GetAsyncIterator / IteratorNext / IteratorClose` | analogous |
| `GetPropertyIterator` / `PropertyIteratorNext` | `ForInEnumerate / ForInNext` |
| `SpreadIntoArray / ArrayPush` | same names v2 |
| `CopyDataProperties / CopyDataPropertiesExcept` | same names v2 |
| `ToNumber / ToString / ToPropertyKey` | analogous acc-form |
| `AssertNotHole / AssertConstructor` | same names v2, acc-form |
| `ThrowConstAssign` | `ThrowConstAssign` |
| `Yield / YieldStar / Await` | same |
| `DynamicImport / ImportMeta` | same |
| `DefineField / DefineComputedField / RunClassFieldInitializer / SetClassFieldInitializer` | same |
| `AllocClassId / CopyClassId` | same |
| `DefinePrivateField / GetPrivateField / SetPrivateField / DefinePrivateMethod / DefinePrivateGetter / DefinePrivateSetter / PushPrivateMethod / PushPrivateGetter / PushPrivateSetter` | same |
| `DefineNamedGetter / DefineNamedSetter / DefineComputedGetter / DefineComputedSetter` | same |
| `DefineClassMethod / DefineClassMethodComputed / DefineClassGetter / DefineClassSetter / DefineClassGetterComputed / DefineClassSetterComputed` | same |
| `SetHomeObject / GetSuperProperty / GetSuperPropertyComputed / SetSuperProperty / SetSuperPropertyComputed` | same |

**No v1 opcode is dropped outright.** Semantics preserved across the migration; only the encoding and accumulator discipline change.

## 8. Risks and open questions (to resolve in Phase 1)

1. **Prefix scope** — confirmed: one prefix byte applies to *all* operands of the immediately-following instruction (Ignition convention). Per-operand prefixing was considered and rejected (decoder complexity).
2. **Jump offset base** — offsets are relative to the *instruction after* the jump (Ignition convention), so `Jump 0` is a true no-op. v1 uses the same convention.
3. **Register-window encoding for calls** — `RegList(base, count)` with both operands same-width. Under `Wide` both become 16-bit.
4. **Feedback slot allocation ordering** — compiler assigns slot ids in emission order. `FeedbackTableLayout` stays sorted; lookups use binary search over a `bytecode_pc → slot_id` vector.
5. **Generator state shape** — the current generator object's register buffer is a `Box<[RegisterValue]>`. In v2 it also needs one extra slot for the saved accumulator. Activation's `save_registers()` grows by one slot; `restore_registers()` consumes it.
6. **JIT deopt materialization** — `JitContext` gains `accumulator_raw: u64` (8 bytes), pushing the struct from 144 → 152 bytes. Offsets in `codegen/lower.rs` update accordingly.
7. **v1 interop during Phase 2–3** — no runtime interop between v1 and v2 bytecodes. The whole program compiles under one version at a time, selected by `Cargo` feature and runtime flag. This keeps the migration simple.

## 9. References

- [Firing up the Ignition interpreter · V8 (2016)](https://v8.dev/blog/ignition-interpreter)
- [Sparkplug — a non-optimizing JavaScript compiler · V8 (2021)](https://v8.dev/blog/sparkplug)
- [Ignition Bytecode Format · Chromium wiki mirror](https://chromium.googlesource.com/external/github.com/v8/v8.wiki/+/69cdcc46450fe609426180fbc5524ea0ecba76d5/Ignition-Bytecode-Format.md)
- [A New Bytecode Format for JavaScriptCore · WebKit (2019)](https://webkit.org/blog/9329/a-new-bytecode-format-for-javascriptcore/)
- Internal plan: `/Users/alexanderstreltsov/.claude/plans/glimmering-dazzling-parasol.md`
- Internal JIT log: `JIT_REFACTOR_PLAN.md` — Phases A / B / B.9 / B.10 landed; v2 is Phase C.
