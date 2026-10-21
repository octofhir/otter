# Otter: port proven engine architecture (fresh session)

Работай автономно, большими пакетами. Если упрёшься в лимит сессии — ставь
таймер (ScheduleWakeup) и продолжай. Коммить промежуточное одним подписанным
коммитом после гейтов.

## Главный принцип этой сессии

ХВАТИТ переоткрывать проблемы через профайлер. V8 / JSC / SpiderMonkey уже прошли
весь этот путь. Для КАЖДОЙ подсистемы ниже:

1. Сначала прочитай реальный исходник эталона (V8 первым, JSC/SM для сверки):
   `gh api search/code`, raw.githubusercontent.com/v8/v8/main/...,
   WebKit/WebKit, mozilla/gecko-dev. Не по памяти — по коду.
2. Запиши дизайн эталона коротко (структуры, раскладка, fast path, slow path,
   инварианты GC/deopt) в `benchmarks/measurements/research-2026-10-06/<subsystem>.md`.
3. Портируй ЭТУ архитектуру целиком в активный стек, без своих эвристик, без
   промежуточных «улучшений текущей схемы». Профилирование — только чтобы выбрать
   порядок работ и замерить результат.

## Состояние на старт

- HEAD: `0dfc65fc9 perf(vm): deopted functions re-warm in baseline; final call failures stay final`.
- НЕзакоммиченный WIP (корректен, частично прогнан), ПЕРВЫМ ДЕЛОМ догнать гейты и закоммитить:
  - `graph/truncation.rs` (новый, untracked) + `ir.rs`: `Int32AddWrapping/Int32SubWrapping` —
    TurboFan word32-truncation: checked add → wrapping, когда все use усекают и ни один
    frame state не держит значение.
  - `graph/mod.rs`: повторный `remove_trivial_phis` после `untag_phis` (+ чистка `loop_headers`).
  - `graph/builder.rs`: фолдинг константных `JumpIf*` при построении (как Maglev) и
    тождества `x|0, x^0, x<<0, x>>0, x&-1`.
  - arm64 Graph: x22–x28 в аллокаторе (`registers.rs`), сохраняются парами ниже x29
    (`arm64/frame.rs`: `emit_save/emit_restore_saved`, tier root words сдвинуты,
    tail transfer/return восстанавливают; `SpillArea.saved_pairs`, `Allocation::used_gp`,
    `OperationContext.saved_pairs`). Все allocatable регистры по-прежнему clobbered вызовами.
  - x86: `emit_add_sub(.., Option<overflow>)`, wrapping kinds.
  - Уже проверено: otter-jit lib arm64 224/224, x86 (Rosetta) 245/245; выводы fib/mega/ast/zlib/
    crypto(md5=node)/ts(exit 0) совпадают; PTC+register-pressure скрипт = node, в т.ч. под
    `OTTER_GC_STRESS=16 OTTER_GC_VERIFY=1`; runtime tests optimizing_osr, jit_canonical_homes_gc ok.
  - НЕ сделано: x86 CLI e2e под Rosetta (`cargo build --release -p otter-cli --target
    x86_64-apple-darwin`, затем zlib/fib/`benchmarks/results/claude-s1/w/ptc_pressure.js`).
- Числа (instructions retired, `/usr/bin/time -l`), сейчас → эталон:
  fib 1.14G (Bun 0.53) · ast_ctor 4.85G (Bun 2.03) · mega_method 2.75G (Bun 1.87) ·
  ts 118.7G (Bun 21.2) · earley-boyer 80.9G (Bun 17.8) · zlib 94.6G (Node 24.8) ·
  crypto 8.65G (Node 2.93) · log1 startup 198M (Bun 58M).

## Подсистемы: что у нас, что у эталона, что портировать (порядок по выигрышу)

1. **Baseline ICs из feedback в рантайме (ts, все полиморфные программы).**
   У нас: Template запекает максимум 1 мономорфный CacheIR на этапе компиляции, остальное —
   общая таблица (shape, atom) на 2048 записей; на ts 10M вызовов Rust load-property стаба,
   77% из них — попадание per-site PIC уже в Rust. Эталон: V8 Sparkplug зовёт IC builtins с
   (feedback vector, slot): mono map+handler, poly array, megamorphic stub cache
   (`src/ic/accessor-assembler.cc`: LoadIC_Noninlined, HandleLoadICHandlerCase,
   TryProbeStubCache; `src/ic/stub-cache.h`; `src/ic/handler-configuration.h`, LoadHandler
   incl. nonexistent + validity cell). JSC: StructureStubInfo + repatching
   (`bytecode/StructureStubInfo.h`, `jit/Repatch.cpp`). Портировать: feedback-слоты, читаемые
   сгенерированным кодом (общий IC-builtin на изолят), handlers для own/proto/absent/accessor,
   function-receiver (`f.call`, статики конструкторов — сейчас 1.5M Rust-загрузок на ts).
2. **Кадр/конвенция вызова (все бенчи; fib 74 instr/call против ~35).**
   У нас каждый вызов пишет ~27-словный native Frame record (depth, caller_return_pc,
   construct ticket, call_site, machine_roots, ...). Эталон: V8 StandardFrame — fp/lr,
   context, JSFunction, argc; всё остальное выводится из return address через code lookup +
   SafepointTable при stack walk (`src/execution/frames.h/.cc`, `src/codegen/arm64/
   macro-assembler-arm64.cc` EnterFrame/Prologue, `src/builtins/arm64/builtins-arm64.cc`
   JSEntry/InterpreterEntryTrampoline, `src/compiler/backend/arm64/code-generator-arm64.cc`
   AssembleConstructFrame). Глубину — только stack limit. Портировать раскладку и walker.
3. **Construct (`new`): ast, earley, ts.** У нас 2 Rust-вызова на `new` (probe + commit, commit
   обратно резолвит сайт вызывающего через return-pc → safepoint). Эталон: V8 FastNewObject
   builtin (new_target.initial_map, inline allocate), slack tracking в счётчике карты,
   construct feedback пишет ВЫЗЫВАЮЩИЙ в свой слот (`builtins-constructor-gen.cc`,
   `objects/map.h` construction_counter, `ic/call-ic`/CollectConstructFeedback).
4. **instanceof (earley).** У нас Rust стаб. Эталон: InstanceOf IC + OrdinaryHasInstance,
   TurboFan JSNativeContextSpecialization::ReduceJSInstanceOf → JSHasInPrototypeChain
   (цепочка map.prototype в машинном коде).
5. **Строковые ключи (ts).** У нас `obj[str]` → `to_lossy_string` (malloc) + словари с ключами
   из байт (FxHash по байтам, сравнение байт). Эталон: internalized strings с хэшем в заголовке,
   string table lookup без вставки (не найден → свойства нет), NameDictionary по указателям
   (`objects/string-table.cc`, `objects/dictionary.h`, KeyedLoadIC_Megamorphic).
6. **Регистровый аллокатор Graph (zlib, crypto).** У нас на backedge цикла ротация из ~12 mov,
   инварианты живут в spill-слотах. Эталон: Maglev `maglev-regalloc.cc` (phi hints,
   loop-header allocation), TurboFan linear scan с hint'ами и splitting
   (`compiler/backend/register-allocator.cc`). x86: allocatable всего 9 GP — сверить с V8
   `ALLOCATABLE_GENERAL_REGISTERS` (`codegen/x64/register-x64.h`, `arm64/register-arm64.h`).
7. **GC (ts ~8%, earley ~14%).** Сверить scavenger/pretenuring (AllocationSite) с V8
   `heap/scavenger.cc`, `heap/pretenuring-handler.cc`.

## Жёсткие правила (не нарушать)

- Только main, без worktree/веток, без git checkout/reset/stash по WIP (отменять свою правку
  точечно). Без второй сборки HEAD для сравнения.
- Каждый коммит GPG-подписан (`G`), НИКАКОГО Co-Authored-By. Коммит отдельным вызовом после
  проверки вывода тестов. Не коммитить PROMPT_*.md, benchmarks/measurements/, research-*,
  scripts/dev/{t262_tostring_probe.js,rewrite_struct_literal.py,remove_match_arms.py,
  remove_rust_items.py}.
- Никогда `cargo test --workspace`; 1–2 `--test` за запуск; один test262 runner строго
  последовательно. node-compat широко — только по прямой просьбе.
- Без thread_local/static mut/process-global; без feature flags/env-переключателей;
  без мостов/совместимых слоёв/версионирования внутренних контрактов.
- Не ослаблять GC-инварианты, PIC/inline бюджеты ради чисел. FIX FORWARD; все падения — наши.
- Файлы в репо (scripts/dev, benchmarks/results), не /tmp. При нехватке диска — cargo clean
  (сохранить нужные бинарники в benchmarks/results/claude-s1/bin/).
- Комментарии вечные: без фаз/задач/долговых заметок.

## Инструменты

- Инструкции: `/usr/bin/time -l <otter> <fixture>` (fixtures: `benchmarks/fixtures/fixed-work/`,
  ожидаемые stdout в `scripts/dev/fixed-work-semantics.py`).
- `scripts/dev/stubprof.py <sample.txt>` — время по Rust-стабам, вызванным из JIT;
  `scripts/dev/hotasm.py <js> <outdir> [n]` (OTTER/DELAY/SECS) — горячие инструкции JIT;
  `scripts/dev/sample_callers.py`, `subtreeprof.py`, `childprof.py`.
- `--cpu-prof` упирается в лимит сэмплов: интервал ≥5000, columnNumber = байтовый offset pc.
- `--jit-events` режется на 16384 событиях (JIT_DEBUG_EVENT_LIMIT) — для полного прогона
  временно поднять в локальной сборке и вернуть.
- x86: `cargo test --release -p otter-jit --lib --target x86_64-apple-darwin` (Rosetta).

## Финал

Перед завершением: гейты VM/JIT/runtime (точечно), GC stress, test262 (один раннер),
полная матрица против Bun/Node, x86 (Rosetta + Linux/docker), удалить временные
редакторы/пробы/дампы/research-исходники, обновить memory.
