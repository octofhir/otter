# Task 09 — String Core Slice (M3)

## Goal

Land the first vertical string slice end-to-end on the staging stack:
literal allocation, equality, length, display/debug output, and `+`
concatenation, all using a WTF-16 backing store with rope variants from
day one.

This is the first slice that introduces a real value representation. It
sets the canonical tag and string layout that later slices reuse.

## Scope

### JS surface covered

- String literals (`"abc"`, `'abc'`, plain template literals without
  expressions — `\`abc\``).
- `+` between two strings.
- `String.prototype.length` getter (read-only; no setter).
- `===` and `==` for two strings.
- Implicit `ToString` for **already-supported primitives only**:
  `undefined` → `"undefined"`, `null` → `"null"`, booleans, integers
  small enough to print without going through the number slice's full
  formatter (defer the rest to task `11`).
- `console.log` of strings (text path only). Console binding is the
  minimum needed by `otter test` to assert `stdout`.

Not yet:

- numeric / boolean operands on `+`,
- methods other than `length` (covered in task `10`),
- string indexing `s[i]` (deferred to task `10`),
- regular expressions, template expression interpolation,
  tagged templates,
- normalization, `localeCompare`, ICU.

### Representation and invariants

Implement, in `otter-vm::string`:

- `JsString` enum / tagged repr with variants:
  - `Flat16(Arc<[u16]>)` — primary storage; WTF-16 code units
  - `Cons { left, right, len, depth }` — concatenation rope node
  - `Sliced { parent, start, len }` — view into a flat or already-flat
    parent (slicing of cons nodes flattens first)
  - `Thin` — placeholder reserved for the future Latin-1/WTF-16 hybrid;
    document in code that this variant is not constructed during this
    slice but the enum tag is reserved
- `len()` returns the **code-unit** length in O(1).
- `equals(&self, other: &JsString) -> bool` compares by code units; flat
  fast paths first, then iterative DFS comparison without recursion.
- Display/debug output renders WTF-16, surrogates and all, lossless
  through the CLI formatter and trace events.
- `concat(left, right)` always produces a `Cons` node — **never** an
  eager flat copy.
- A single `flatten` function realizes a rope into `Flat16` using an
  iterative DFS with an explicit `Vec` stack and a `MAX_ROPE_DEPTH`
  constant. Document the constant value and the rationale in code.
- `slice(parent, start, len)` produces a `Sliced` view; if the parent is
  a `Cons` node, it flattens first. Subslices of `Sliced` collapse into a
  single `Sliced` view (no `Sliced(Sliced(...))` chains).
- `Arc<[u16]>` is the only owned heap path for flat strings. No
  `String` (UTF-8) heap allocation in the string subsystem.
- `from_str(&str) -> JsString` exists for diagnostics / source-derived
  strings; converts UTF-8 → UTF-16 once at construction.
- `from_utf16(units: &[u16]) -> JsString` is the canonical constructor
  for parser-produced literals.

### Heap accounting

- Each `Flat16` allocation goes through a fallible `alloc_string(units)`
  helper that:
  1. checks the runtime heap cap **before** mutation,
  2. returns `Err(VmError::OutOfMemory)` if the allocation would exceed
     the cap (no partial work, no panic),
  3. on success, increments the runtime's tracked byte counter by
     `units.len() * 2` plus a fixed header overhead.
- `Cons` and `Sliced` carry a small fixed accounting overhead;
  document the constants.
- A unit test creates a runtime with a 4 KiB heap cap, allocates strings
  until the cap is hit, and asserts the allocation that would exceed the
  cap returns `OutOfMemory` and the heap byte counter is **unchanged**
  for that failed call.

### Bytecode

Add the minimum opcodes:

- `LoadConstString <reg> <const_index>` — loads from the function's
  constant table; the constant table stores `Arc<[u16]>` directly.
- `Equal <dst> <a> <b>` — for two strings, code-unit equality;
  for not-yet-supported pairs, falls through to a `TypeMismatch` runtime
  error so the failure is loud, not silent.
- `StrictEqual <dst> <a> <b>` — same as `Equal` for two strings; tag
  comparison short-circuit when `a` and `b` point to the same `Cons`/
  `Flat16`.
- `Concat <dst> <a> <b>` — string-only at this slice; emits a `Cons`.
- `LoadLength <dst> <src>` — string-only at this slice; reads `len()`.

Each opcode entry in `otter-bytecode` must carry the spec fields
required by task `06`: mnemonic, operands, register effect, span policy,
interrupt policy (none for any of these), allocation behavior.

### Compiler integration

Lower:

- string literals → `LoadConstString`
- string `+` → `Concat`
- `s.length` (where receiver type is statically guaranteed string in
  the foundation subset — only literal string constants and string-
  typed locals proven by the compiler's narrow analysis) → `LoadLength`
- `===` / `==` between strings → `StrictEqual` / `Equal`
- everything else still rejects with the existing "feature not in this
  slice" diagnostic

The compiler does **not** introduce a wrapper-object IC path here.
Primitive receiver lookup for full `String.prototype` arrives in task
`10`; until then, only `length` works on strings, and only via the
explicit `LoadLength` opcode.

### Public API / CLI

- `ExecutionResult::completion` can carry a `Value::String(JsString)`.
- The CLI rendering for `otter -p` of a string (when the value model
  exposes a printable type) uses the formatter from task `06` —
  surrogates survive.
- `console.log(s)` on a string runs through stdout; the byte sequence is
  the WTF-16 string lossily encoded to UTF-8 with replacement for lone
  surrogates **only at the stdout boundary**, not internally. Document
  the rule.

### Tests

Engine fixtures under `tests/engine/strings/`:

- `literal-eq.ts` — `"abc" === "abc"`
- `literal-len.ts` — `"abc".length === 3`
- `concat-binary.ts` — `"a" + "b" === "ab"`
- `concat-loop.ts` — 1000-iteration `s += piece` loop completes in
  measurable time and `s.length === 4000`. Asserts no O(n²) flatten.
- `surrogate-roundtrip.ts` — string built from `𐀀` parts
  preserves both surrogates through length and equality.
- `oom-string-alloc.ts` — runs with `--max-heap-bytes 4096`, attempts a
  string allocation that exceeds the cap, asserts the runtime returns
  the catchable `RangeError`-equivalent diagnostic.

Snapshot tests:

- Disassembly for `concat-binary.ts`.
- JSON dump for `literal-eq.ts`.

Benchmarks (`crates-next/otter-vm/benches/strings.rs`):

- `literal_load` — load 1000 string constants, no concat
- `equality_eq_short` — equality on 16-char strings
- `concat_loop_1k` — 1000-iteration `+=` loop
- `flatten_balanced` — flatten a balanced cons tree of depth 16
- `length_after_concat` — repeated `len()` calls on cons nodes

Each benchmark records its input size, the expected classification per
the foundation plan ("smoke" or "regression gate"), and a target
comparison (Otter previous baseline; node/bun/deno deferred until the
slice is more capable).

## Out of scope

- `String.prototype` methods other than `length`.
- String indexing.
- ToString for non-string operands of `+` (number/object handling lives
  in tasks `11` and beyond).
- Latin-1 specialization. The `Thin` variant is reserved but not
  constructed.
- ICU, normalization, locale-aware comparison.

## Files / directories you may touch

- Edit / create under `crates-next/otter-vm/`,
  `crates-next/otter-compiler/`,
  `crates-next/otter-bytecode/`,
  `crates-next/otter-runtime/`,
  `crates-next/otter-cli/`
- Create fixtures under `tests/engine/strings/`
- Create benchmark targets under `crates-next/otter-vm/benches/`

You **must not** modify `crates/*` from this task, and you **must not**
copy code out of `crates/*` either. The new engine is written from
scratch (ADR-0001 §8). Reading legacy code as reference is fine.

## Acceptance criteria

- All `tests/engine/strings/*.ts` fixtures pass under `otter test
  --suite engine` with the declared outcomes.
- `concat_loop_1k` completes in O(n) (loop runtime ratio for `n=1k` vs
  `n=2k` is < 3.0× on a developer machine; record the measured value
  in the slice's update notes).
- The OOM fixture proves the heap counter does not change on a rejected
  allocation (covered by a Rust unit test, not the JS fixture).
- Surrogates round-trip through equality, length, and `console.log`.
- Disassembly and JSON dump golden files match.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- No `String` (UTF-8) heap allocation in the new string subsystem.
- The `Thin` variant is defined and documented but never constructed.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine \
    --filter strings/
cargo bench -p otter-vm --bench strings -- --quick
rg -n 'String::with_capacity|String::from\(' \
    crates-next/otter-vm/src/string \
    && exit 1 || true   # no UTF-8 heap allocation in the string subsystem
```

## Risks

- **Eager concat creep.** Any helper that "just collects bytes" risks
  becoming an O(n²) trap. Audit every new helper.
- **Recursive flatten.** Recursion in `flatten` is forbidden — pathological
  cons chains would stack-overflow. Use an explicit stack.
- **Hidden UTF-8 round-trip.** Display/Debug paths can sneak UTF-8 in.
  Keep one boundary (stdout) and document it.
- **OOM partial mutation.** A failed allocation that already updated
  the byte counter is a silent bug; the unit test for this is not
  optional.

## Next task

Proceed to [`10-string-methods-slice.md`](./10-string-methods-slice.md).

## Status

- **done** (foundation subset; benchmarks + advanced surrogate
  fixtures deferred to slice 10)
- last update: 2026-04-26
- artifacts:
  - `crates-next/otter-vm/src/string.rs` — full `JsString` model:
    `Flat(Arc<[u16]>)`, `Cons { left, right, len, depth }`,
    `Sliced { parent, start, len }`, reserved `Thin(Arc<[u8]>)` (not
    constructed). `MAX_ROPE_DEPTH = 64`. Iterative DFS walker for
    `flatten`, `equals`, `to_lossy_string`, `char_code_at`. Auto-
    flatten on `concat` when depth budget would overflow. `slice`
    over `Cons` flattens once; over `Sliced` collapses.
    `StringHeap` with atomic CAS-loop reservation; OOM never mutates
    the counter.
  - `crates-next/otter-vm/src/intrinsics.rs` — declarative caркас
    для primitive-receiver методов: `IntrinsicReceiver`,
    `IntrinsicArgs`, `IntrinsicEntry`, `IntrinsicTable`,
    `IntrinsicError`, плюс макрос `intrinsics!` для табличных
    регистраций. Используется slice 10+; в текущем slice — только
    типы и unit-тесты.
  - `Value` теперь enum `{ Undefined, String(JsString) }`; больше не
    `Copy`. `Value::PartialEq` сравнивает строки через
    `JsString::equals`.
  - `otter-bytecode`: новые опкоды `LoadString`, `StringConcat`,
    `StringEq`, `LoadLength` + `Operand::ConstIndex(u32)` +
    модульная константная таблица `BytecodeModule.constants`
    (`Constant::String { utf16 }`). Disasm печатает `k[idx]`.
  - `otter-compiler`: `FunctionContext` со scratch-аллокатором и
    interning constant pool. `compile_expr` для `StringLiteral`,
    template literal без интерполяции, `+`/`===` (string-only),
    `.length`, parens. TS-erasure из task 08 сохранён.
  - `otter-vm` dispatch loop добавляет соответствующие arm'ы;
    интерпретатор владеет `Arc<StringHeap>`. `VmError` расширен
    `TypeMismatch` и `OutOfMemory`.
  - 6 фикстур под `tests/engine/strings/`: `literal-load`,
    `literal-eq`, `concat-binary`, `length-on-literal`,
    `template-no-interpolation`, `concat-loop-stays-linear`.
- verification:
  - `cargo build/test/clippy/fmt` — все зелёные.
  - **53 unit-теста** в workspace (compiler 19, vm 18, runtime 17,
    bytecode 4, syntax 4, test 2). string subsystem unit-тестов:
    13 (включая surrogate round-trip, OOM-без-мутации,
    deep-rope flatten, slice-of-slice collapse).
  - `cargo run -p otter-cli -- test --suite engine` — **17/17
    PASS** (2 smoke + 6 strings + 9 typescript).
  - `otter -p '"hello".length'` → `5`.
  - `otter -p '"a" + "b" + "c"'` → `abc`.
  - `otter --dump-bytecode tests/engine/strings/concat-binary.ts` —
    эмитит LOAD_STRING / STRING_CONCAT / RETURN с корректными span.
  - LLM-friendly `//!` headers — все `.rs` файлы.
- design highlights:
  - `+` в этом slice — **только строка-строка**; компилятор смешивает
    их через `StringConcat`, который требует обоих string-операндов
    на runtime (TypeMismatch если нет). Полноценный `+` с
    coercion прибудет в slice 11/12.
  - `===` тоже string-only сейчас. Boolean заводится в slice 12;
    сейчас результат материализуется временно как строка `"true"` /
    `"false"` чтобы тесты могли видеть значение через `-p`.
  - `.length` пока возвращает строку с десятичным числом по той же
    причине; slice 11 заменит на реальный `Number`.
- deferred (попадут в slice 10 / позже):
  - benchmark target `crates-next/otter-vm/benches/strings.rs` —
    Criterion suite (`literal_load`, `equality_eq_short`,
    `concat_loop_1k`, `flatten_balanced`, `length_after_concat`)
    запланирован в slice 10 рядом с реальными методами.
  - `s += piece` loop fixture — нужны переменные (slice 12).
  - explicit `lone surrogate` fixture в `tests/engine/strings/` —
    нужен escape-литерал в `.ts` (отложено).
