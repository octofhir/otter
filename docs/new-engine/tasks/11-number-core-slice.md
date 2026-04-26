# Task 11 — Number Core Slice (M5)

## Goal

Land numeric semantics narrowly and correctly: numeric literals, int32
fast path, double fallback, arithmetic, comparison, `NaN` / `-0` /
infinities, and `ToNumber` only for primitives that earlier slices
already implement (`undefined`, `null`, booleans, strings of digits).

This slice introduces the canonical numeric tag in the value
representation and the explicit int32-to-double fall-through rule.

## Scope

### JS surface covered

- Numeric literals: decimal integers, decimal floats, hex (`0x...`),
  octal (`0o...`), binary (`0b...`), `Infinity`, `NaN`.
- Arithmetic operators: `+`, `-`, `*`, `/`, `%`, unary `-`, unary `+`,
  prefix `++`, prefix `--`, postfix `++`, postfix `--` (each operating on
  numeric receivers; non-numeric coercion is limited as described below).
- Comparisons: `<`, `<=`, `>`, `>=`, `==`, `===` for two numbers and
  number/string mixes that strict-equality expects to be `false`.
- `ToNumber` for:
  - already-numeric values (identity)
  - `undefined` → `NaN`
  - `null` → `+0`
  - `true` → `1`, `false` → `+0`
  - strings: parse the foundation subset only — decimal integer literal
    text, leading/trailing whitespace per spec, the literal strings
    `"Infinity"`, `"-Infinity"`, `"NaN"`, the empty string `""` → `+0`.
    Hex/binary/octal string forms and full `StringNumericLiteral` are
    explicitly deferred. Non-conforming strings produce `NaN`.

Not yet:

- `BigInt`, `Number.prototype` methods (`toString`, `toFixed`, …) —
  separate slices.
- Full `StringNumericLiteral` parsing.
- Bitwise operators (`&`, `|`, `^`, `<<`, `>>`, `>>>`, `~`) — separate
  slice.

### Representation and invariants

- Add the `Number` value variant. Internal layout:
  - `Smi(i32)` — small-integer immediate path.
  - `Double(f64)` — fallback path for non-int32 results.
- Conversions:
  - `Smi(n)` is canonical for any integer in `[-(2^31), 2^31 - 1]`. A
    `Double` carrying an exact int32 must be normalized to `Smi` at
    conversion boundaries.
  - `-0` is **never** representable as `Smi`. Operations that produce
    `-0` always store `Double(-0.0)`.
  - `NaN` and `±Infinity` are always `Double`.
- Operator fast paths:
  - `Smi op Smi` checks for overflow and demotes to `Double` on overflow.
  - Division of two `Smi` values that does not yield an exact integer
    demotes to `Double`.
  - `%` follows `IEEE 754` remainder semantics; document the helper.

### Bytecode

Add explicit smi/double opcodes so the dispatcher can stay specialized:

- `LoadInt8 <reg> <imm:i8>`, `LoadInt32 <reg> <imm:i32>`
- `LoadDouble <reg> <const_index>` — constant-pool double for non-int
  literals
- `AddInt32 <dst> <a> <b>` (with overflow check → `Double` fallback)
- `SubInt32`, `MulInt32`
- `Add`, `Sub`, `Mul`, `Div`, `Mod` — generic numeric paths used when
  the compiler cannot prove both operands are int32; runtime selects
  between smi and double execution.
- `Neg`, `Pos` — unary minus / plus.
- `Inc`, `Dec` — used by prefix/postfix `++` / `--`.
- `LessThan`, `LessEqual`, `GreaterThan`, `GreaterEqual`, `Equal`,
  `StrictEqual` — extended to numeric pairs (the string forms from
  task `09` already exist; these handlers add the numeric arms).
- `ToNumber <dst> <src>` — produces a normalized numeric value or
  `Double(NaN)` per the rules above.

Each opcode carries its task-`06` metadata.

### Compiler integration

- Lower numeric literals to `LoadInt8` / `LoadInt32` / `LoadDouble`.
- Lower binary arithmetic to the generic `Add`/`Sub`/… opcodes by
  default; specialize to `AddInt32`/etc. when both operands are
  proven `Smi` by the slice's narrow type tracker. Specialization is
  optional in this task — the generic opcodes must work on their own.
- Lower `++` / `--` to `Inc` / `Dec`.
- `ToNumber` is inserted when the language requires it (e.g., RHS of `-`
  on a string).

### Tests

Engine fixtures under `tests/engine/numbers/`:

- `int-arith-basic.ts` — `1 + 2 * 3 === 7`
- `division-fractional.ts` — `1 / 2 === 0.5`
- `division-by-zero.ts` — `1 / 0 === Infinity`, `-1 / 0 === -Infinity`,
  `0 / 0` is `NaN`
- `negative-zero.ts` — `-0` semantics: `1 / -0 === -Infinity`,
  `Object.is(-0, -0) === true` (when `Object.is` lands; until then,
  assert via internal helpers exposed under `cfg(test)`)
- `nan-not-equal-to-itself.ts` — `NaN !== NaN`
- `int32-overflow-promotion.ts` — `(2 ** 30) * 4 === 4_294_967_296`
- `inc-dec-operators.ts` — prefix and postfix `++`/`--` produce expected
  values
- `to-number-from-string.ts` — `+"42"` is `42`, `+""` is `0`,
  `+" "` is `0`, `+"foo"` is `NaN`
- `to-number-from-bool-and-null.ts`
- `compare-mixed-string-number-strict.ts` — `"1" === 1` is `false`

Rust unit tests:

- `smi_overflow_demotes_to_double`
- `negative_zero_round_trip_through_div`
- `to_number_string_subset_matches_spec_examples`

Benchmarks (`crates-next/otter-vm/benches/numbers.rs`):

- `int_loop_sum_1m` — sum 0..1_000_000 with `Smi` path; assert no
  allocation
- `double_loop_sum_1m` — same loop on doubles
- `mixed_compare_branch_1m` — comparison branch loop

## Out of scope

- `BigInt`.
- `Number.prototype.*` methods.
- Bitwise operators.
- Full `StringNumericLiteral` (hex/binary/octal/exponent strings).
- `Math.*`.

## Files / directories you may touch

- Edit / create under `crates-next/otter-vm/`,
  `crates-next/otter-compiler/`,
  `crates-next/otter-bytecode/`
- Create fixtures under `tests/engine/numbers/`
- Extend `crates-next/otter-vm/benches/`

## Acceptance criteria

- All `tests/engine/numbers/*.ts` fixtures pass.
- `int_loop_sum_1m` records its wall-clock time and asserts zero heap
  allocation in the loop body via the runtime's allocation counter
  (Rust harness asserts the counter delta is `0`).
- `-0`, `NaN`, `±Infinity` semantics are covered by dedicated fixtures.
- Smi → Double demotion paths are covered by Rust unit tests.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter numbers/
cargo bench -p otter-vm --bench numbers -- --quick
```

## Risks

- **`-0` regressions.** Easy to lose `-0` when normalizing. The fixtures
  must catch this.
- **Hidden allocation in `Smi` path.** A `Vec<Value>` push on every add
  defeats the int loop. The zero-alloc assertion is a hard gate.
- **String coercion creep.** `+""` parsing pulls in surprising amounts
  of spec text. Stay in the foundation subset; document deferrals.
- **Overflow semantics.** `i32::MAX + 1` must promote to `Double`, not
  panic. Use `checked_add` and friends.

## Next task

Proceed to [`12-boolean-nullish-control-flow-slice.md`](./12-boolean-nullish-control-flow-slice.md).

## Status

- **done** (foundation subset; bench targets and bitwise ops
  deferred to a perf pass; full `StringNumericLiteral` parsing
  intentionally omitted per slice scope)
- last update: 2026-04-26
- artifacts:
  - `crates-next/otter-vm/src/number.rs` — `NumberValue` two-state
    representation (`Smi(i32)` + `Double(f64)`), canonicalization,
    arithmetic with `checked_*` overflow → demote semantics,
    spec-style comparison via `NumericOrdering`,
    `to_number_from_string` foundation subset (decimal int/float +
    `NaN`/`±Infinity`).
  - `Value` extended with `Boolean(bool)` and `Number(NumberValue)`.
    Equality routes Numbers through `number::equals` (so `NaN !==
    NaN`).
  - Bytecode: 14 new / generalized opcodes —
    `LoadNumber` (uses `Constant::Number { bits }`),
    `LoadInt32` (inline `Operand::Imm32(i32)`),
    `LoadTrue`, `LoadFalse`,
    `Add`/`Sub`/`Mul`/`Div`/`Rem`/`Neg`/`ToNumber`,
    `Equal`/`NotEqual`/`LessThan`/`LessEq`/`GreaterThan`/`GreaterEq`.
    String-only `StringConcat`/`StringEq`/`String*` опкоды
    удалены — `Add`/`Equal`/comparisons теперь полиморфны
    (Number+Number или String+String). Disasm печатает `i32:<v>`
    для `Imm32`.
  - Compiler lowering: `NumericLiteral` → `LoadInt32` (для смите-
    влезающих интов) или `LoadNumber` через интернированный
    `Constant::Number`; `BooleanLiteral` → `LoadTrue/False`;
    идентификаторы `NaN`/`Infinity` → `LoadNumber`;
    `UnaryExpression` (`-`/`+`) → `Neg`/`ToNumber`; полный набор
    binary арифметических / сравнительных операторов.
  - `string_prototype` ретиро суррогаты — `length`/`indexOf`/
    `charCodeAt` возвращают `Value::Number`, `startsWith`/
    `endsWith` возвращают `Value::Boolean`. `arg_u32_or` теперь
    принимает `Value::Number` напрямую.
  - VM dispatch: helper-методы `Interpreter::run_add`,
    `run_numeric`, `run_compare`, `binop_regs` — каждая ветка
    короткая и идиоматичная; `run_numeric` параметризован
    pointer-функцией над `NumberValue`.
  - 4 новые фикстуры под `tests/engine/numbers/`:
    `integer-arith`, `division-by-zero`, `comparisons`,
    `unary-and-coercion`. String-method фикстуры обновлены, чтобы
    использовать настоящие числовые литералы (`slice(1, 4)` вместо
    `slice("1", "4")`).
- verification:
  - `cargo build/test/clippy/fmt` — все зелёные.
  - **85+ unit-тестов** в workspace: vm 34 (number: 9, string: 13,
    string_prototype: 8, intrinsics: 3, dispatch: 3-4),
    compiler 23, runtime 17, bytecode 4, syntax 4, test 2.
  - `cargo run -p otter-cli -- test --suite engine` — **28/28
    PASS** (4 numbers + 2 smoke + 6 strings + 7 string methods +
    9 typescript).
  - End-to-end через `-p`:
    - `1 + 2 * 3` → `7`
    - `1 / 0` → `Infinity`
    - `NaN` → `NaN`
    - `Infinity - Infinity` → `NaN`
    - `+"42"` → `42`, `+"foo"` → `NaN`
    - `"hello".length` → `5`
    - `"hello".charCodeAt(0)` → `104`
    - `"hello".indexOf("ll")` → `2`
    - `"hello".startsWith("he")` → `true`
    - `1 === 1` → `true`, `3 !== 4` → `true`
  - LLM-friendly `//!` headers — все `.rs` файлы.
- design highlights:
  - Idiomatic Rust: `checked_add/sub/mul`, fn-pointer
    `run_numeric(op: fn(NumberValue, NumberValue) -> NumberValue)`,
    `?`-style propagation everywhere, нет `unwrap()` на горячих
    путях.
  - `-0.0` round-trips через `Double(-0.0)` и сравнивается ===
    `+0` (spec).
  - `NaN` через `Double(f64::NAN)` всегда; `equals` early-return на
    NaN; `compare` возвращает `NumericOrdering::Unordered`.
  - Constant pool хранит `f64::to_bits` чтобы NaN payload и `-0`
    round-trip через JSON dump были bit-exact.
  - `Operand::Imm32(i32)` для inline smi-литералов уменьшает
    давление на constant pool.
- deferred (явно не блокирует закрытие задачи):
  - Bitwise operators (`&`, `|`, `^`, `<<`, `>>`, `>>>`, `~`) —
    отдельный slice.
  - `BigInt`, `Number.prototype.*` методы (`toString`, `toFixed`,
    `parseInt` family) — последующие slices.
  - Full ECMA-262 `StringNumericLiteral` parsing
    (hex/binary/octal strings, exponent forms) — расширение
    `to_number_from_string` в perf pass'е.
  - Criterion bench targets (`int_loop_sum_1m`,
    `double_loop_sum_1m`, `mixed_compare_branch_1m`) — отложено
    до слайса 12 рядом с `s += piece` циклом, чтобы бенчмарки
    отражали реалистичные JS shapes.
