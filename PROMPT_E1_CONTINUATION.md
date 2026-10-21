# Продолжение серии E1 (окружения) — Otter VM/JIT/GC

Работай в `/Users/alexanderstreltsov/work/octofhir/otter`, ветка `main`, как ведущий
архитектор VM, JIT/AOT и GC. Это продолжение сессии 2026-09-27/28; исходная
постановка — `PROMPT_ENGINE_RESEARCH_REFACTOR.md`, решение и контракт —
`benchmarks/measurements/2026-09-27-architecture.md` (разделы 1–8 + Checkpoint)
и `benchmarks/measurements/2026-09-27-environments-contract.md`.

## Темп работы (главное)
- Широкие архитектурные шаги, а не микропроценты. Не полировать ±1–2%;
  брать изменения, которые двигают нагрузки на десятки процентов или убирают
  целый механизм.
- Проверка лёгкая по ходу: `otter-jit --lib`, difftest, 1–3 затронутые нагрузки.
  Тяжёлые гейты (Test262 `language/`, интеграционные JIT-тесты `otter-runtime`,
  все 7 нагрузок) — один раз перед коммитом крупного шага, не после каждой правки.
  На прогоны — не больше ~15% времени.
- Коммитить крупными законченными шагами.

## Правила (без исключений)
- Только `main`: без веток, worktree, stash, второй сборки HEAD для сравнения.
- Коммиты подписанные (проверяй `git log --show-signature -1` → `G`), без
  `Co-Authored-By` и любых AI-трейлеров. GPG не отключать.
- `PROMPT_OCTANE_GAP.md` не трогать и не коммитить. Чужие изменения не удалять.
- Без feature-флагов, адаптеров, параллельных движков, legacy-режимов; заменённый
  код удалять.
- Никогда `cargo test --workspace`; не собирать пачку интеграционных тестов
  `otter-runtime` без необходимости — ~100 бинарей, это десятки минут и диск.
  Если нужно — только конкретные `--test <имя>` по одному-двум.
- Один test262-раннер за раз; полный Test262 не гонять без причины.
- Замеры — fixed-work инструкции (`scripts/dev/fixed-work.py`), не очки Octane;
  не запускать cargo параллельно с замером.
- Диск ~93%: после крупных сборок `find target/release/deps -name '*.rcgu.o' -delete`.

## Состояние на момент паузы
Последний коммит: `6a09147d`. **E1a в рабочем дереве, НЕ закоммичен** (≈190 файлов:
bytecode/compiler/vm/gc/runtime/jit + новые `context*.rs`, `binding_emit.rs`,
корпуса difftest). Всё собирается, релизный CLI `target/release/otter` актуален.

Проверено на текущем дереве:
- `cargo test --release -q -p otter-jit --lib` — 303/303.
- `cargo run --release -q -p otter-difftest` — 80/80 (интерпретатор-оракул,
  Template, production, GC stress 1/4/16) →
  `benchmarks/results/arch-2026-09-27/e1/gate-difftest-3.json`.
- 7 fixed-work нагрузок (`benchmarks/results/arch-2026-09-27/e1a-m5`), vs HEAD
  `ceb3ca18`: ts −7.1%, zlib −9.4%, crypto −0.9%, fib −4.4%, mega −1.1%,
  ast_ctor −13.1%, earley −14.6%. RSS earley 135 → 153 MB (+14%) — открыт.
- Test262 `language/` 24,077/24,077 — прогнан ДО последних трёх JIT-фиксов.

Последние фиксы этой сессии (описаны в разделе 8 отчёта):
1. Template: inline hit-путь checked context slot load/store (arm64+x86),
   `TemplateOp::BindingValue.context_coord`; hole → committed-вызов.
2. `arguments_elision::analyze` игнорирует closure-context placeholder операнды
   (`closure_ctx_patches`), иначе CJS-функции материализовали `arguments`.
3. Machine OSR: трамплин пишет входы в локации ДО `Before`-edits
   (`regalloc::locations_before_edits`) и входит перед ними (arm64+x86);
   корпус `osr_entry_live_values.js`.
4. Map/Set natives: flatten rope-ключа до чтения receiver; `-p` completion
   рутится через event loop.

Не проверено (прервано пользователем): интеграционные JIT-тесты `otter-runtime`
(`--test 'jit_*'`). В прошлой итерации там было 24 падения (ожидания старой
upvalue-модели: `jit_inline_method_guard_miss` ждёт ≤1 bail, `jit_machine_elements`
ждёт регионы "upvalue read" и т.п.).

## Что делать дальше (по порядку)
1. Точечно (по одному `--test`) прогнать упавшие ранее JIT-тесты otter-runtime,
   обновить устаревшие ожидания под контекстную модель или чинить реальные баги.
   Список имён — `benchmarks/results/arch-2026-09-27/e1/gate-runtime-jit.txt`
   (частичный) и `grep -l "upvalue" crates/otter-runtime/tests/jit_*.rs`.
2. Test262 `language/` одним раннером (после JIT-фиксов), затем подписанный
   коммит E1a (отдельными коммитами можно: Map/Set GC-фикс, `-p` rooting,
   OSR-фикс). Обновить Checkpoint в отчёте.
3. E1b: размер контекста (убрать слово `extension` у скоупов без sloppy eval, как
   в V8), молодые замыкания, inline-аллокация контекстов/замыканий в Template и
   Machine, свёртка singleton-контекстов; цель — RSS earley ≤ HEAD и дальнейшее
   снижение инструкций. Замер 7 нагрузок + census до/после.
4. Потом: call/frame контракт, object layout, AOT-срез (Template-код с
   таблично-относительными вызовами стабов → relocatable артефакт →
   standalone executable), расследование timing инициализации полей
   derived-классов (P0-3).

После каждого принятого изменения: перепрофилировать и идти к следующему
крупнейшему разрыву. Если сессия упирается в лимит — оставь точный checkpoint в
отчёте и продолжай.
