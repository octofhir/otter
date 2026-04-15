# Plan: Переход на чистый v2-runtime, без флагов, production-ready, cross-platform

## Context

Текущее состояние — гибридный рантайм: AST → v1 bytecode (~11k LOC `source_compiler/`) → опциональный транспилятор в v2 bytecode → v2 interp / v2 JIT. v2 прячется за feature flag `bytecode_v2` и env var `OTTER_V2_TRANSPILE`. Попытка склеить v1 и v2 через транспилятор накопила баги (стенсиль JIT диверджит на реальном коде, deopt-thrash, TypeError в v2 interp). **Решение — выбросить v1 целиком, включая тесты, и построить v2 с нуля как единственный рантайм.**

Ключевые требования пользователя:

1. Без feature flag'ов. v2-код всегда компилируется, всегда работает.
2. Нормальный нейминг — никаких `_v2`-суффиксов. Канонические имена (`bytecode`, `dispatch`, `source_compiler`).
3. v1 остаётся в git history как reference — в рабочем дереве удалён. Можно посмотреть `git show 316d5da:crates/otter-vm/src/source_compiler/compiler.rs` при необходимости.
4. Старые тесты (unit, integration, test262, node-compat) **удаляем**. Новые пишем с нуля под v2, сразу после каждого milestone.
5. Код максимально кросс-платформенный (macOS + Linux + Windows; aarch64 + x86_64) и production-ready (без unwrap/panic в публичных путях, без TODO/FIXME в shipped-коде, все error-ветки осмысленные).
6. Step by step — каждый milestone = 1 коммит с работающим end-to-end примером и зелёными тестами.

Пока v2 source compiler не покрывает нужный AST — скрипты падают с `SourceLoweringError::Unsupported { construct, span }`. Это осознанное ограничение, **не баг**.

---

## Что и где трекать

- **Этот файл** (`~/.claude/plans/eager-foraging-dongarra.md`) — design-документ: цели, milestones, критерии готовности, промт для coding-агента. Claude-private, не в репо.
- **Новый файл `V2_MIGRATION.md` в корне репо** — публичный tracker прогресса (создаётся в M0 первым шагом). Краткий, без истории. Содержит:
  - Таблица milestones с галочками `[x]` / `[ ]`.
  - Текущее покрытие AST-конструкций v2-компилятором.
  - Ссылки на коммиты для завершённых milestones.
  - Бенчмарки vs bun/node по мере роста.
- **Существующий `JIT_REFACTOR_PLAN.md`** — исторический design-документ. В Implementation Log добавляем одну строку итога каждого milestone, не раздуваем.

---

## Quality bar (действует с M0 и дальше)

1. **Cross-platform.**
   - Interpreter — pure Rust, работает на всех target'ах.
   - JIT сейчас только `aarch64`. x86_64 backend добавляется как `M_JIT_x86_64` после того как interpreter-путь стабилен. До тех пор на `x86_64`/`Windows` primary-путь — interpreter; JIT выключен. Никогда `panic!("unsupported arch")` в рантайме.
   - В CI минимум `cargo build --target` зелёный для `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.
2. **Без `unwrap()` / `expect()` / `panic!()`** в non-test коде. Все ошибки — `Result<_, SourceLoweringError>` или эквивалент. В тестах `unwrap` допустим.
3. **Ни одного `TODO` / `FIXME` / `unimplemented!()` в shipped-коде.** Если feature не готова — возвращаем `Unsupported { construct, span }`. Это контракт, а не заглушка.
4. **Public API задокументирован.** Каждый `pub fn` имеет doc-comment с примером или ссылкой на тест.
5. **Тесты на каждый код-путь** включая error-ветки.
6. **Deterministic output.** Где важен порядок (JSON, сериализация, iteration) — `BTreeMap` / `IndexMap`, не `HashMap`.
7. **Depth limits** на рекурсивные алгоритмы (AST walker) — явный `MAX_DEPTH` + graceful `Unsupported { construct: "nesting too deep" }`.
8. **CI gate после каждого milestone:**

   ```bash
   timeout 180 cargo build --workspace
   timeout 90  cargo clippy --workspace --all-targets -- -D warnings
   timeout 30  cargo fmt --all --check
   timeout 180 cargo test --workspace
   ```

   Все четыре зелёные. Плюс: `cargo build --target <каждая из 4 платформ>` когда toolchain доступен.

---

## Milestones

### M0 — Чистка: удаляем v1, удаляем флаги, канонизируем имена, создаём tracker

Задачи:

1. **Создать `V2_MIGRATION.md`** в корне репо — пустой шаблон трекера.
2. **Удалить v1 целиком:**
   - `crates/otter-vm/src/source_compiler/` — директория удаляется.
   - `crates/otter-vm/src/bytecode.rs` — удаляется.
   - `crates/otter-vm/src/interpreter/dispatch.rs` — удаляется.
   - `crates/otter-vm/src/bytecode_v2/transpile.rs` — удаляется (мост).
   - `crates/otter-vm/src/module.rs::maybe_attach_v2_bytecode` — удаляется, поле v1 `bytecode` удаляется, `bytecode_v2` переименовывается в `bytecode`.
   - `crates/otter-jit/src/baseline/mod.rs` + v1 MIR/CLIF машинерия (`mir/`, `codegen/`, `osr_compile.rs`, `deopt/`, `runtime_helpers.rs`) — **прочитать сначала** какие типы из них импортирует v2 JIT (`baseline/v2.rs`, `pipeline.rs`, `tier_up_hook.rs`). Что используется — оставляем (`arch/`, `code_memory`, `telemetry`, `config`, `BailoutReason`, `BAILOUT_SENTINEL`, `JitContext`); что v1-only — удаляем.
   - Env var `OTTER_V2_TRANSPILE` — удаляется.
   - Cargo feature `bytecode_v2` из всех `Cargo.toml` — удаляется, `#[cfg(feature = "bytecode_v2")]` гейты снимаются.
3. **Удалить старые тесты:**
   - `tests/node-compat/`, `tests/test262_*` — удаляются целиком.
   - `crates/*/tests/` integration-тесты — удаляются.
   - Unit-тесты старого `source_compiler` / `dispatch` — удаляются вместе с файлами.
   - Сохраняются ТОЛЬКО тесты: v2 ISA (encoding/decoding в `bytecode/tests.rs`), v2 interpreter примитивов (в `dispatch.rs::tests`), v2 JIT emitter (disassembly smoke test в `baseline/mod.rs::tests`).
4. **Канонический rename:**
   - `crates/otter-vm/src/bytecode_v2/` → `crates/otter-vm/src/bytecode/`.
   - `crates/otter-vm/src/interpreter/dispatch_v2.rs` → `crates/otter-vm/src/interpreter/dispatch.rs`.
   - `crates/otter-jit/src/baseline/v2.rs` → `crates/otter-jit/src/baseline/mod.rs`.
   - `Function::bytecode_v2` → `Function::bytecode`.
   - Все внутренние типы: `V2TemplateInstruction` → `TemplateInstruction`, `dispatch_v2::step_v2` → `dispatch::step`, и т.д.
5. **Scaffold нового source compiler:**
   - `crates/otter-vm/src/source_compiler/mod.rs` — `ModuleCompiler::compile(program, source) -> Result<Module, SourceLoweringError>` (stub, всегда `Err(Unsupported { construct: "program", span })`).
   - `crates/otter-vm/src/source_compiler/error.rs` — `SourceLoweringError` enum.
   - `crates/otter-vm/src/source_compiler/tests.rs` — тест: парсим `""`, получаем `Unsupported`.
6. **CLI:** `otterjs run foo.ts` использует новый `ModuleCompiler`. После M0 любой реальный JS падает с понятной `Unsupported`-ошибкой — ожидаемое состояние до M1.

Verify (полный quality gate):

```bash
timeout 180 cargo build --workspace
timeout 90  cargo clippy --workspace --all-targets -- -D warnings
timeout 30  cargo fmt --all --check
timeout 180 cargo test --workspace
./target/debug/otter --help
./target/debug/otter run /tmp/any.ts  # падает с Unsupported, exit != 0
```

Commit: `chore(vm): retire v1 bytecode + legacy tests, canonicalize v2 naming, add V2_MIGRATION tracker`

---

### M1 — `function f(n) { return n + 1 }` end-to-end

Scope ровно:

- `Program` с одной `FunctionDeclaration`. Остальное — `Unsupported`.
- `FunctionDeclaration`: имя (Identifier), 0–1 параметров (Identifier, без default/rest/destructuring), тело — `BlockStatement` с одним `ReturnStatement`. async/generator — `Unsupported`.
- `Expression::Identifier` — ссылка на параметр через `FrameLayout::resolve_user_visible`.
- `Expression::NumericLiteral` — int32-safe (`fract() == 0.0 && в [i32::MIN, i32::MAX]`); иначе `Unsupported`.
- `Expression::BinaryExpression` с `+` и обоими int32-safe. Если правая сторона — literal ∈ [i8::MIN, i8::MAX], `AddSmi`; иначе `Add` с регистром (и материализацией литерала в slot).

Emit для `function f(n) { return n + 1 }`:

```text
Ldar r0        ; acc = n
AddSmi 1       ; acc = n + 1
Return
```

Новые тесты:

- `f_n_plus_1_returns_43_when_n_is_42`.
- `f_without_params_returns_literal` — `function g() { return 7; }` → 7.
- `class_is_unsupported` — `class Foo {}` → `Unsupported { construct: "class_declaration" }`.
- `non_int32_literal_is_unsupported` — `function h() { return 1.5; }` → `Unsupported`.
- `two_functions_unsupported_in_m1` — `function a(){} function b(){}` → `Unsupported`.

Verify — полный quality gate + smoke `./target/release/otter run m1.ts` где `m1.ts` = `function f(n) { return n + 1; }; f(42)` (после M_globals это сработает, в M1 top-level call `f(42)` падает с `Unsupported` — тест через внутренний API, не CLI).

Обновить `V2_MIGRATION.md` и `JIT_REFACTOR_PLAN.md::Implementation Log`.

Commit: `feat(vm): native v2 compiler handles function f(n) { return n + 1 }`

---

### M2 — Benchmark M1 + JIT stencil sanity

- Integration test `#[test] #[ignore]`: 2000 warmup + таймер на 1M вызовов `f(42)` через internal API.
- `#[test]`: `OTTER_JIT_DUMP_ASM=1` smoke — disassembly стенсиля содержит ровно `ldr x21, eor, tst, b.ne, sxtw, mov x10, add, sxtw, box_int32, ret`, размер ≤ 200 байт.
- `V2_MIGRATION.md`: таблица latency (aarch64 interp, aarch64 JIT).

Commit: `test(jit): v2 stencil disassembly sanity + M1 microbenchmark`

---

### M3+ — Инкрементальный рост AST

Каждый milestone = 1 коммит, 1 AST-узел, тесты, обновление `V2_MIGRATION.md`, quality gate.

Черновой порядок:

- **M3** — остальные int32 бинарники: `-`, `*`, `|`, `&`, `^`, `<<`, `>>`, `>>>`. Scope: `function f(n){ return (n + 1) | 0 }`.
- **M4** — локальные `let`/`const` с initializer.
- **M5** — `AssignmentExpression` (=, +=, -=, *=, |=) на локальный let.
- **M6** — `IfStatement` + relational ops (<, >, <=, >=, ===, !==) для int32.
- **M7** — `WhileStatement`. Закрывает bench2.ts: `function sum(n){ let s=0,i=0; while(i<n){ s=(s+i)|0; i=i+1; } return s; }`. Полный бенчмарк vs bun/node.
- **M8** — `ForStatement` (desugar → while + init + update).
- **M9** — несколько функций + `CallExpression` без `this`/closures.
- **M_JIT_x86_64** — x86_64 backend к JIT baseline.
- **M10+** — closures, globals, `console.log`, classes, async, generators, destructuring, property access, exceptions, exports/imports. По приоритету реальных use-cases.

---

## Критические файлы (M0)

| Путь | Операция |
| --- | --- |
| `V2_MIGRATION.md` (new) | **создать** |
| `crates/otter-vm/src/source_compiler/` (v1) | **удалить директорию** |
| `crates/otter-vm/src/bytecode.rs` (v1 ISA) | **удалить** |
| `crates/otter-vm/src/interpreter/dispatch.rs` (v1) | **удалить** |
| `crates/otter-vm/src/bytecode_v2/transpile.rs` | **удалить** |
| `crates/otter-vm/src/bytecode_v2/` | **rename → `bytecode/`** |
| `crates/otter-vm/src/interpreter/dispatch_v2.rs` | **rename → `dispatch.rs`** |
| `crates/otter-jit/src/baseline/v2.rs` | **rename → `baseline/mod.rs`** |
| `crates/otter-jit/src/baseline/mod.rs` (v1) | **удалить перед rename** |
| `crates/otter-jit/src/{mir,codegen,osr_compile,deopt,runtime_helpers}` | **оценить зависимости v2 JIT; удалить что v1-only** |
| `crates/otter-vm/src/module.rs` | удалить `maybe_attach_v2_bytecode`, swap полей `bytecode`↔`bytecode_v2` |
| все `Cargo.toml` (workspace + члены) | удалить feature `bytecode_v2` |
| `crates/otterjs/src/*` | убрать ссылки на feature / env var / v1 pipeline |
| `tests/node-compat/` | **удалить** |
| `tests/test262_*` | **удалить** |
| `crates/*/tests/*` (integration) | **удалить** |
| `crates/otter-vm/src/source_compiler/` (new scaffold) | **создать в конце M0** |

---

## Coding Agent Prompt

Скопировать целиком и подать как первое сообщение новой coding-сессии (Claude Code / coding-агент). Промт самодостаточный — агент сможет начать M0, не читая этот файл полностью (он всё равно прочитает нужные файлы как ссылки).

```prompt
You are a Rust architect with Node.js core-maintainer experience picked up from V8 / JSC / SpiderMonkey internals. Your job is to retire a hybrid JS bytecode pipeline (v1 legacy + v2 new) in a Rust-based JavaScript runtime called OtterJS and rebuild it as a clean v2-only stack — step by step, production-ready, cross-platform, without feature flags or `_v2` naming noise.

# Project

Repository: /Users/alexanderstreltsov/work/octofhir/otter
Git branch: main
Crates: otter-gc, otter-vm, otter-jit, otter-runtime, otter-modules, otter-web, otter-nodejs (parked), otterjs (CLI).
Current working tree has uncommitted changes from a previous session attempting Phase 4.5b of a JIT refactor — read `git status` and revert only if the changes conflict with M0 below.

# What you're replacing

- v1 pipeline: AST → v1 bytecode (~11k LOC in `crates/otter-vm/src/source_compiler/`) → v1 dispatch (`crates/otter-vm/src/interpreter/dispatch.rs`) → v1 JIT baseline (`crates/otter-jit/src/baseline/mod.rs` + MIR/CLIF in `mir/`, `codegen/`, `osr_compile.rs`, `deopt/`, `runtime_helpers.rs`).
- v2 pipeline (partial): AST → v1 → transpile bridge (`crates/otter-vm/src/bytecode_v2/transpile.rs`) → v2 bytecode → v2 dispatch (`crates/otter-vm/src/interpreter/dispatch_v2.rs`) → v2 JIT baseline (`crates/otter-jit/src/baseline/v2.rs`). Gated behind feature `bytecode_v2` and env `OTTER_V2_TRANSPILE`.
- The transpile bridge accumulated bugs in the JIT stencil for real source-compiled code (LdaThis / LdaCurrentClosure / ToNumber sequences diverge). We are NOT debugging it — we are deleting it.

# Your mandate

Execute milestones M0 → M1 → M2 → ... from the design plan. Exactly one milestone per commit. Each commit must (a) pass the full quality gate, (b) update `V2_MIGRATION.md` (created in M0) with a `[x]` and commit hash, (c) add ONE LINE to `JIT_REFACTOR_PLAN.md` Implementation Log summarizing the milestone.

**Start with M0 (cleanup + rename + scaffold). Do not proceed to M1 until the user says so.**

# First read (in order)

1. `git status` and `git log --oneline -20` — understand current uncommitted work and recent history.
2. `JIT_REFACTOR_PLAN.md` — skim "Current State (2026-04-15)" and the last 5 Implementation Log entries. Ignore the rest.
3. `docs/bytecode-v2.md` — canonical v2 ISA spec. Keep open.
4. `CLAUDE.md` + `AGENTS.md` — project conventions (macros, `unsafe` discipline, GC invariants, capability model).
5. `/Users/alexanderstreltsov/.claude/projects/-Users-alexanderstreltsov-work-octofhir-otter/memory/MEMORY.md` — user feedback patterns, past bug diagnoses, file layout notes.
6. `crates/otter-vm/src/bytecode_v2/` (becoming `bytecode/` in M0) and `crates/otter-vm/src/interpreter/dispatch_v2.rs` (becoming `dispatch.rs`) — canonical v2 surfaces you must preserve.
7. `crates/otter-jit/src/baseline/v2.rs` (becoming `baseline/mod.rs`) — the one piece of v2 JIT infrastructure worth keeping; has `AccState` tracker and bailout pads wired to `JitContext.accumulator_raw`.

# Non-negotiables

- **Cross-platform.** Interpreter works everywhere (aarch64 + x86_64, macOS + Linux + Windows). JIT stays aarch64-only initially, with graceful fallback to interpreter on other archs — never `panic!("unsupported arch")`. `cargo build --target` must succeed on aarch64-apple-darwin, x86_64-apple-darwin, x86_64-unknown-linux-gnu, x86_64-pc-windows-msvc.
- **No `unwrap()` / `expect()` / `panic!()` / `unimplemented!()` in non-test code.** Use `Result<_, SourceLoweringError>` (or analog). `unwrap` only inside `#[cfg(test)]`.
- **No `TODO` / `FIXME` in shipped code.** Feature not ready → `Unsupported { construct: &'static str, span: oxc_span::Span }`.
- **Public API is documented.** Every `pub fn` has a doc comment with example or test reference.
- **No `unsafe` without a `// SAFETY:` comment** describing invariants. Prefer safe abstractions; `unsafe` only at the FFI boundary into JIT code.
- **Deterministic collections.** `BTreeMap` / `IndexMap` where output order matters; `FxHashMap` / `rustc_hash` where order is irrelevant and hot-path performance matters.
- **Depth limits** on recursive AST traversal — concrete `MAX_DEPTH` constant + `Unsupported { construct: "nesting too deep" }`.

# Quality gate (green after every commit)

```
timeout 180 cargo build --workspace
timeout 90  cargo clippy --workspace --all-targets -- -D warnings
timeout 30  cargo fmt --all --check
timeout 180 cargo test --workspace
```

On aarch64 hosts additionally run the JIT unit tests. On other hosts only interpreter path tests should exist and pass.

# Working discipline

- Use TodoWrite to break each milestone into subtasks (~5–10 items). Mark complete immediately, not in batches.
- Commit per milestone. Format: `<type>(<crate>): <what> — <why in one line>`. Types: `feat`, `chore`, `refactor`, `fix`, `test`, `bench`, `docs`. Include Co-Authored-By footer.
- Run regression with `timeout` ALWAYS. On macOS Apple Silicon, bare `cargo test` can deadlock on mmap'd JIT code; the timeout kills it cleanly.
- NEVER run test262 unless the user explicitly asks.
- NEVER run `cargo test --release` for regression — debug is faster and catches more bugs.
- NEVER invoke compiled JIT stencils directly from Rust test harnesses on macOS — use the CLI binary or the production `TierUpHook::execute_cached` path. Direct invocation hangs with uninterruptible-exiting zombies.
- NEVER use destructive git ops (reset --hard, push --force, stash pop without checking) without explicit user permission.
- NEVER bypass pre-commit hooks (`--no-verify`).

# If you hit an obstacle

- If M0 uncovers a v1 dependency you can't cleanly untangle in one commit (e.g., v2 JIT silently depends on a v1 MIR type) — stop and report. Don't bandage.
- If a rename creates a symbol collision you can't resolve trivially — stop, report options, let user pick.
- If a test you're deleting is actually testing a v2 code path (not v1) — keep it and port, don't delete silently.
- If your guarded emitter produces a stencil that diverges at runtime — disassemble via `crates/otter-jit/src/codegen/disasm.rs` before guessing. Use `OTTER_JIT_DUMP_ASM=1`.

# M0 scope (your first commit)

Read `~/.claude/plans/eager-foraging-dongarra.md` section "Milestones → M0" for the authoritative list. Summary:

1. Create `V2_MIGRATION.md` in repo root (empty tracker template with M0..M10 table).
2. Delete v1: `source_compiler/`, `bytecode.rs`, `interpreter/dispatch.rs`, `bytecode_v2/transpile.rs`, `module.rs::maybe_attach_v2_bytecode`, v1 JIT (`baseline/mod.rs` + v1-only MIR machinery after auditing v2 JIT deps), `OTTER_V2_TRANSPILE` env, `bytecode_v2` feature + all `#[cfg(feature = "bytecode_v2")]` gates.
3. Delete old tests: `tests/node-compat/`, `tests/test262_*`, all `crates/*/tests/` integration tests, unit tests of deleted files. Keep v2 ISA/interp/JIT-emitter unit tests.
4. Rename: `bytecode_v2/` → `bytecode/`, `dispatch_v2.rs` → `dispatch.rs`, `baseline/v2.rs` → `baseline/mod.rs`, `Function::bytecode_v2` → `Function::bytecode`, internal types (`V2TemplateInstruction` → `TemplateInstruction` etc).
5. Scaffold `crates/otter-vm/src/source_compiler/{mod,error,tests}.rs` with stub `ModuleCompiler::compile` that always returns `Err(Unsupported { construct: "program", span })`.
6. CLI wires into new `ModuleCompiler`. Any real JS script fails with clear `Unsupported` — expected until M1.

After M0, quality gate green, commit, update both tracker files, STOP and wait for user to say "proceed to M1".

# How to know you're done with M0

- `cargo build --workspace` zero warnings (clippy -D warnings passes).
- `cargo test --workspace` passes (very small test surface; only v2 ISA encoding, v2 dispatch primitives, v2 JIT emitter disassembly).
- `./target/debug/otter --help` works.
- `./target/debug/otter run /tmp/smoke.ts` (where smoke.ts is `function f(){return 1;}`) fails with `SourceLoweringError::Unsupported { construct: "program", ... }` and a non-zero exit code.
- Git tree: zero uncommitted changes. One new commit on top of current HEAD.
- `V2_MIGRATION.md` shows `[x] M0` with the commit hash.
- `JIT_REFACTOR_PLAN.md` Implementation Log has one new line for M0.

Go.
```

---

## Status

- [x] Plan approved by user.
- [ ] M0 committed (agent executes this).
- [ ] M1 committed.
- [ ] M2 committed.
- [ ] M3+ — incremental (see `V2_MIGRATION.md` after M0).
