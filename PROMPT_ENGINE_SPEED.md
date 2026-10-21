# Ускорение движка Otter: интерпретатор, вызовы, компилятор

Цель: Otter должен стать радикально быстрее. Браузер Otlyra — только
градусник; вся работа в этом репозитории. Мелкие правки на проценты не
нужны. Нужны архитектурные изменения, каждое с измеренной дельтой.

## 0. Сначала: незакоммиченный порт oxc 0.154

В дереве лежит готовый, но незакоммиченный переход oxc 0.139 → 0.154.

- `Cargo.toml` и `Cargo.lock`.
- Новый модуль `crates/otter-compiler/src/body.rs`: `Body` — блок функции или
  тело стрелочной функции, у которой тело может быть выражением.
- Правки в compiler, runtime, pm-phantom и node (codegen).
- Что изменилось в AST:
  - `MetaProperty` заменён на `ImportMeta` и `NewTarget`;
  - `Class.super_class` заменён на `heritage`;
  - `export` разделён на три узла: `ExportDeclaration`,
    `ExportNamedDeclaration`, `ExportFromDeclaration`;
  - `TSModuleDeclaration` разделён на `TSExternalModuleDeclaration`,
    `TSNamespaceDeclaration` и `TSGlobalDeclaration`.

Порядок действий:
1. Проверить итог полного прогона: `test262_results/full-run-2026-10-18e.log`.
   Эталон — 52460 pass / 15 fail, известный набор: intl402 ×9,
   import-defer super-get ×2, update-to-dynamic-import, staging annex-b
   arguments ×2, internalUsage.
2. Если прогон не завершён, перезапустить:
   `bash scripts/test262-full-run.sh > test262_results/full-run-<date>.log`.
   Запускать в фоне с `timeout: 7200000`, только один раннер.
3. Новые падения починить. Затем запустить `cargo fmt` на изменённых
   листовых файлах и `cargo clippy -p otter-compiler -p otter-runtime --lib -- -D warnings`.
4. Закоммитить. Цифры: парсинг 71.1 → 61.4 мс на наборе бандлов, бандл
   YouTube 10.8 МБ 79.4 → 71.4 мс (`scripts/dev/oxc_parse_bench`).
5. Положить `scripts/dev/native_hot.py` в отдельный коммит или в тот же.

## 1. Цикл диспатча интерпретатора — главный рычаг

Замеры: `--interpreter`, `/usr/bin/time -l`, разница числа retired
instructions для n=0 и n=1e6.

- Цикл `s += i` (5 опкодов: LT, JUMP_IF_FALSE, ADD, INCREMENT, JUMP) —
  **766 инструкций на итерацию**, то есть ~150 на опкод.
- Цикл с вызовом `s += f(i, 1)` — **3568 инструкций на итерацию**.
- По времени вызов в интерпретаторе стоит ~220 нс, у `node --jitless` — 47 нс.
- Пробы: `scripts/dev/probe/interp_calls_only.js` и
  `scripts/dev/probe/interp_dispatch_cost.js`. Числа ниже — на эквиваленте
  `calls_n.js` / `loop_n.js` с аргументом n.

Где тратится (`crates/otter-vm/src/interp/dispatch.rs`, `dispatch_ticks`;
дизассемблировать через `objdump --disassemble-symbols`):

- **Пролог каждого тика, ~60 инструкций до перехода по таблице:**
  - `stack.top_with_depth()`: загрузка `ctx.native_frame`, проверка флага
    `COMPLETED`, `skip_host_frames`, `logical_depth`;
  - сравнение кэша `(function_id, depth)`;
  - проверка границы индекса инструкции;
  - `function.source_work().charge(1)` — счётчик в памяти на каждую
    инструкцию, когда установлен JIT;
  - `feedback_recorder_at(idx)` — поиск по таблице на каждой инструкции;
  - `work_budget_stats.record_work(instr.reductions())` на каждой инструкции.
- **Обработчики** (`run_add_regs` и другие) заново вычисляют верхний кадр
  через `stack.top_unchecked()` / `stack[top_idx]`. `ActivationStack::index`
  каждый раз проходит по цепочке кадров, считая глубину.

Что сделать (одной архитектурной правкой):
- Держать указатели на кадр, функцию и инструкцию в локальных переменных
  цикла. Переразрешать их только при смене кадра: дешёвая проверка
  `ctx.native_frame == cached`.
- Копить `source_work` и work-счётчики в локальных переменных. Сбрасывать
  их на обратных переходах, вызовах, возвратах, throw и выходе из
  `dispatch_ticks`. Инвариант tier policy «каждый опкод учтён один раз» и
  все точки решений (OSR на back-edge, вход в вызов) должны видеть
  актуальную сумму.
- В обработчики передавать указатель на окно регистров, без повторного
  поиска кадра. Байткод верифицирован, поэтому доступ к регистрам без
  проверок допустим там, где верификатор это гарантирует.
- Цель: ≤ 300 инструкций на итерацию цикла (сейчас 766). Проверять
  `cargo test -p otter-vm` (точечно), test262 и `OTTER_GC_STRESS` на
  пробах с аллокациями.

## 2. Вход в интерпретатор при вызове

Модель «один кадр на вызов» сохраняется: кадр строит пролог в трамплине.
Сейчас каждый вызов выглядит так: ход вызывающего → asm-трамплин → новый
`interpreter_turn` вызываемого (`crates/otter-vm/src/interp/call_dispatch.rs`)
→ возврат → ещё один `interpreter_turn` вызывающего.

Что дорого на свежем входе:
- `activation.owner_context()` → `foreign_owner`. Все лениво
  скомпилированные тела живут в своих чанках, поэтому почти любой вызов
  «чужой»: клон `ExecutionContext` — 2 Arc inc/dec — и `RefCell`
  `OwnerCache` на каждый ход.
- `begin_suspendable_activation` и `prepare_compiled_entry` повторяют
  `for_function` и `exec_function`.
- Проверки retraining, asm-модуля и JIT-входа.
- `take_request` и копии `Option<CallRequest>` по значению.
- `BytecodeArgumentWindow::to_smallvec8`.

Что сделать:
- Разрешать владельца один раз и без Arc-оборота: стабильная ссылка на
  контекст чанка, кэшированная в кадре или в активации.
- Предвычислить флаги функции (генератор, asm, есть код, retraining) и
  пропускать проверки одним битовым тестом.
- Убрать лишние копии запроса и аргументов.
- Цель: вызов в интерпретаторе ≤ 1200 инструкций на итерацию (сейчас 3568).

## 3. Компилятор

Профиль ленивого компилятора, бандлы YouTube:
- парсинг oxc — 45%;
- анализ захватов (`capture::Builder`) — 30%;
- `StrictValidator` — 11%;
- `BlockLexicalValidator` — 6%.

Это три отдельных полных обхода AST. Что сделать:
- Слить проверки валидаторов в обход захватов, получив один проход вместо трёх.
- Цель: −15% `lower` в `otter-web-bundles compile` (YouTube сейчас 165 мс,
  в режиме `--eager` lower 332 мс).

oxc сначала пробует разобрать `(x=…)` как параметры стрелочной функции и
откатывается. На обфусцированном коде это даёт ~3x (recaptcha 25 мс против
~8 мс на сопоставимый объём). Исправление — только патч или апстрим oxc,
обсудить отдельно.

## 4. Прочее, по убыванию пользы

- `f(...args)`: NEW_ARRAY + SPREAD_APPEND + CALL_SPREAD создают временный
  массив на каждый вызов (120 нс против 11 у node). `f.apply(o, arr)` —
  68 нс против 5: двойной проход через трамплин.
- Фоновая подготовка модулей, аналог `PreparedScript`
  (`crates/otter-runtime/src/code_cache.rs`) для `WebModuleMap`.
- GC — ~5% загрузки страницы.

## Инструменты

- `scripts/dev/launchprof.py <dir> 1 -- <cmd>` — профиль xctrace
  (`THREAD=""` — все потоки, `FOCUS=`, `ATTRIBUTE=`).
- `scripts/dev/prof_top.py <xml> N [--under RE] [--filter RE]`.
- `scripts/dev/native_hot.py <xml> <symbol>` — горячие адреса нативной
  функции. Для строк исходника нужны `.o`: не удалять `*.rcgu.o` перед этим.
- `otter-web-bundles` (`cargo build --release -p otter-benchmark --features engine --bin otter-web-bundles`):
  `compile [--eager]` по `benchmarks/fixtures/web-bundles`.
- `scripts/dev/otlyra_code_cache.sh` и `scripts/dev/otlyra_load.sh` — только
  как итоговый градусник по реальным сайтам. Требуют `OTLYRA_NO_KEYCHAIN=1`;
  делать ≥ 2 чередующихся прогона и записывать load average.

## Правила (из памяти проекта)

- **Ветки и коммиты:**
  - только `main`, без веток и worktree;
  - коммиты локальные, подписанные (проверять `G`);
  - `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com"`;
  - никаких Co-Authored-By;
  - коммит отдельной командой после проверки вывода тестов.
- **Содержимое кода и коммитов:**
  - в коде и коммитах не называть другие движки;
  - без флагов и env-переключателей;
  - без `thread_local` и процессного глобального состояния;
  - без «осталось/не сделано» в сообщениях коммитов.
- **Тесты:**
  - не плодить тесты, расширять существующие;
  - `cargo test -p otter-runtime` — только 1–2 `--test`;
  - никогда `--workspace`;
  - test262 — один раннер.
- **Диск:**
  - заполнен на ~97%;
  - `df -h /System/Volumes/Data` перед сборками;
  - при < 15 ГБ чистить `target/*/incremental` и `*.rcgu.o`;
  - `cargo clean --target` не использовать.
- **Измерения:** в каждом коммите цифры до/после (инструкции, мс). Итоговые
  числа по сайтам — в конце.
