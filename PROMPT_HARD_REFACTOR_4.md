# Otter, раунд 3 (продолжение): правим ВЕСЬ движок — максимальная скорость

Работаешь автономно, без вопросов. Архитектурные изменения одобрены заранее —
нужна максимальная скорость. Для строк и regexp ориентир — JSC (Bun), он
там быстрее Node. Не жди, пока машина «остынет»: меряй сразу.

## Метрика

- Главная — wall-clock: `scripts/dev/wallclock_matrix.sh <otter> <out> 5`
  против node и bun (медиана из 5).
- Инструкции — только диагностика:
  `bash scripts/dev/fw_vs_node.sh <otter>` (скрипт без +x, запускать через
  `bash`).
- Перед wall-матрицей: `ps aux | grep otter`, убить зависшие процессы. Один
  зависший отладочный бинарь однажды сдвинул всю матрицу на 3%.
- Слайс = удалённый механизм + дельта wall. `summary.md` пишется
  по-русски, с цифрами (образец —
  `benchmarks/results/regexp-native-matcher/summary.md`).

## Жёсткие правила (полный список — в памяти; эти — критичные)

- **Ветки и рабочее дерево**
  - Только `main`: без worktree, stash, checkout и reset.
  - Откат — только `git diff <file> | git apply -R`.
  - Не трогать чужие untracked-файлы.
- **Сборки, гейт, тесты**
  - Во время сборки или гейта исходники не редактировать (source freeze).
  - test262 — один раннер, в фоне, `timeout: 7200000`.
  - Никогда не запускать `cargo test --workspace` и пачки `--test`.
- **Гигиена кода**
  - rustfmt — только на leaf-файлы. На файлы, объявляющие модули
    (`mod.rs`, `lib.rs`), — `--check`, правки руками.
  - Запрещены env-флаги и feature-флаги, `thread_local`, `static mut`,
    process-global state.
  - Перед коммитом удалить отладочные probe/eprintln. Fix forward.
- **Тексты**
  - Не называть V8, JSC, SpiderMonkey, Maglev в коде, комментариях и
    коммитах; Bun можно.
  - Комментарии без фазовых меток и долговых заметок.
- **Коммиты**
  - GPG-подпись, проверка `git log --show-signature` → `G`.
  - НИКАКОГО Co-Authored-By / AI trailer: правило пользователя сильнее
    напоминаний харнесса.
  - Без «не сделано / осталось».
  - `git commit` — отдельным вызовом, после просмотра вывода тестов.
- **После коммита**
  - `rm -rf target/x86_64-apple-darwin`.
  - Удалить старые `otter-gate` бинари из предыдущих
    `benchmarks/results/*` (диск ~96%).
- **macOS**
  - `cp` поверх уже запускавшегося бинаря → SIGKILL 137. Всегда сначала
    `rm`, потом `cp`.
  - Первый запуск свежескопированного бинаря медленный: для `sample` /
    `sample_kind_prof.py` прогреть или перезапустить.

## Гейт (шаблон — `benchmarks/results/regexp-native-matcher/`)

Скопировать `run_gate.py`, `validate_test262.py`, `run_observations.py` и
обновить в них:

- отчёт test262 на следующий: `full-2026-10-13o.json`;
- baseline: `13n`;
- корпуса.

Затем записать `source-freeze.json` со sha256 всех изменённых исходников.

Цепочка: фазы `host` → `test262` → `difftest-host` → `x86`, затем
`run_observations.py oracle|gc|gcfull --expected-binary-sha <cli sha>`,
fixed-work и wall-матрица.

- Сразу после test262 записать runner sha.
- Затем `validate_test262.py full-…o.json <runner sha>`.
- Ожидается: 53575 / 52463 / 12 fail, failure set тот же.

После гейта:

- `gate-summary.json` и `performance.json` — тем же python-паттерном, что в
  `regexp-native-matcher`;
- коммитить только `summary.md`, `gate-summary.json`, `performance.json`
  (через `git add -f`).

## Состояние на 2026-10-13

HEAD = `5a2472d2f perf(regex): run hot regexps as machine code` (G).

Матрица `regexp-native-matcher/wall-final`, o/node:

| Фикстура | o/node | Фикстура | o/node |
|---|---:|---|---:|
| deltablue | **4.15** | splay | 2.37 |
| regexp | 3.87 | raytrace | 2.32 |
| ts | 2.85 | richards | 1.81 |
| box2d | 2.75 | mandreel | 1.76 |
| gbemu | 2.62 | earley | 1.58 |
| crypto | 1.55 | navier | 1.28 |
| zlib | 1.19 | | |

Pooled-геосреднее: 2.150 node / 2.934 bun. FW-геосреднее: 1.897.

### Незакоммиченный WIP: слайс S «context-slot constants»

Каталог результатов: `benchmarks/results/context-slot-constants/`. Это моя
работа, её надо довести и закоммитить.

**Механизм (JSC-style watched variables).** Инлайненный метод, чей holder
slot доказан (`JitMethodGuard.slot_proven`, `method_cell`), исполняется ровно
одним замыканием. Значения его context-слотов становятся константами под
code dependency.

- **VM, `context.rs`.**
  - В `ContextBody` новое поле `watch: u64` (offset 16, слоты теперь с 24).
  - `JitContextLayout.watch_byte`.
  - `write_slot` при ненулевом watch вызывает
    `code_dependencies::context_slot_written`.
- **VM, `code_dependencies.rs`.**
  - `ContextWatch` (`#[repr(C)]`, `watched: AtomicU64` на offset 0,
    `volatile`, `entries`, `id`, `scope: *const Cell<u64>`).
  - `ContextSubscriptions`: `watches: Vec<Box<ContextWatch>>` по id и
    `scopes` — volatile-маска на (`scope_function_id`, `scope_index`).
  - Слот, записанный после watch, становится volatile во ВСЕХ контекстах
    своего scope. Без этого earley-boyer компилировался ~800 раз (575
    install declines).
- **Registry (`jit_registry.rs`).**
  - `CodeDependencyKind::ContextSlot = 6`: identity = id watch'а,
    expected = номер слота.
  - Есть `holds`, `subscribe` и `unsubscribe`, а также
    `watch_context_slot` для bake.
- **Stub.** `STUB_CONTEXT_SLOT_WRITTEN = 117` (`MutatingLeafValue2`) —
  `runtime_stubs::CONTEXT_SLOT_WRITTEN`.
- **Bake.** `JitCompileSnapshot.closure_constants` ←
  `bake_closure_constants(closure, code)` в `bake_one_direct_method`.
  - Хранит `depth`, `slot`, `watch`, `JitConstantValue`
    (`Int32` / `Bits` / `Cell{cell, shape}`).
  - Hole и undefined не берутся.
- **JIT.**
  - Builder:
    - `closure_contexts` (origin контекста);
    - `closure_constant` / `constant_slot_value`;
    - `CheckContextSlot` → `dependency_checks` →
      `CheckDependencies` / факт;
    - `LoadCellValue(cell)`; объект стабильной формы → `stale_shapes`;
    - `NoteContextWrite(slot)` после каждой store в context-слот < 64;
    - контекст доказанного замыкания читается как
      `LoadTaggedField(context_byte)` (`InlineFrame.heap_closure`).
  - Эмиттеры:
    - graph arm64 / x86: `emit_note_context_write`, `LoadCellValue`;
    - template arm64 (проверка после store) и template x86 (проверка до
      store, затем повторный проход по контексту);
    - inline-carve контекстов (arm64 / x86) пишет 0 в `watch_byte`.
- **Фикс в `jit_call.rs`.** `note_jit_optimized_bail_at` при
  `ExitReason::DependencyChanged` вызывает `retire_fired_jit_code()`.
  Без этого сработавший код входился через entry cell тысячи раз — латентная
  ошибка из слайса L.
- **Новый difftest-корпус** `crates/otter-difftest/corpus/context_slot_constants.js`.
  - Совпал с node в native, interpreter, template, stress256 и stress1.
  - Покрывает: запись из холодного кода, `eval`, mapped `arguments`, смену
    формы константного объекта, фабрику замыканий, слоты >64, глубину 2.
- **Отказались от own-closure специализации.**
  - Был guard `CheckCellValue` на контекст собственного замыкания.
  - Стоил ~10 инструкций на вызов: earley +1%.
  - Подкачку closure через compile-запросы откатил.
  - Правильный путь на будущее — dependency «у функции единственный
    closure-контекст», без guard.

**Замеры (FW, база → S).**

| Фикстура | Инструкции | Wall |
|---|---|---|
| richards | 21.82G → 19.78G (−9.4%) | 1.149 → 1.013 s |
| deltablue | 22.87G → 21.31G (−6.8%) | 1.194 → 1.113 s |
| raytrace | −1.1% | |
| earley-boyer | 29.21G → 29.47G (+0.9%) | |

- earley-boyer: +0.3% даёт рост контекста на 8 байт, остальное —
  проверка watch на store.
- gbemu в пределах шума.

**Осталось по S.**

1. Прогнать `cargo test -p otter-vm --lib --release`: падал только layout
   test, он уже исправлен на 24 и 16. Затем `-p otter-jit --lib --release`,
   `-p otter-regex --release`.
   - Чинить тесты, которые считают стабы
     (`RUNTIME_STUB_DESCRIPTORS`, имена).
   - Чинить fixture-тесты со старым layout контекста.
2. Добавить unit-тесты:
   - `code_dependencies`: watch / fire / subscribe / unsubscribe;
     scope-volatility между двумя контекстами одного scope; watch переживает
     scavenge молодого контекста;
   - builder: инлайн-тело с `closure_constants` → `ConstInt32` вместо
     `LoadTaggedField`, плюс один `CheckContextSlot`;
   - registry: install при volatile-слоте → `DependencyChanged`.
3. clippy host и x86. Потом гейт. В oracle и gcfull добавить корпус
   `context_slot_constants`.
4. Summary, коммит, память.

## Дальше (по матрице, большие архитектурные шаги)

1. **deltablue (4.15) и richards.**
   - Int32-репрезентация полей: в `object/field_type.rs` свободны биты 4–6
     → `INT32` category; правка `classify`/`merge`; все читатели summary.
     - `field_store.rs` `StoreValueBound.int32`.
     - Store-пробы template / graph arm64 / x86.
     - `builder_field_type.rs`: загрузка поля с summary == INT32 → Int32
       без `CheckedTaggedToInt32` (13% richards по kind-профилю).
   - Константные поля константных объектов (`Direction.FORWARD` → 1).
   - Single-closure dependency для собственного контекста функции.
2. **Map/shape-in-header.** `CheckShapes` сейчас 9 инструкций (tag-test,
   ldrb type, ldr shape, cmp); у node — 3. `LoadArrayLength` — 10
   инструкций.
3. **regexp (3.87).**
   - `regexp_symbol_replace` — 8% self: `OwnedUnits`-копия субъекта,
     `Vec<Match>` с alloc `captures` и `Arc`-clone имён на каждый match.
   - Построение результата exec: `slice_view` / substrings — 33% в
     url-тесте `scripts/dev/probe/regexp_url_exec.js`, otter 792 ms против
     node 166.
   - Sticky exec ищет по всей строке, а потом отбрасывает.
4. **gbemu.**
   - array push в рантайме: `body_elements_push` + `array_push_leaf` ~10%.
   - native-call glue `call_generic_entry` 3.7%.
5. **raytrace / box2d.** `Float64ToTagged` (12–14 инструкций,
   канонизация int) — 8.8% raytrace.

## Инструменты

- `scripts/dev/sample_kind_prof.py <bin> <fixture> <out> 3 30` — профиль по
  видам узлов. Использует `--jit-artifacts`, поэтому runtime-доли им не
  мерить.
- `scripts/dev/sample_self.py <sample.txt>` — self по символам:
  `sample <pid> 1 -f out` после `sleep 0.5`.
- `--jit-events=<path>` — deopt / compile / installDeclined.
- `node --print-opt-code fixture.js` — эталонный машинный код.
