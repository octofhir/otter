# Otter, раунд 3: продолжение — объектная модель и доступ к свойствам, НЕ regexp

Работай автономно, без запросов аппрува, до завершённых, проверенных и
подписанных коммитов. Уже сделанное пропускай.

Репозиторий: /Users/alexanderstreltsov/work/octofhir/otter

## Сначала прочитай

1. AGENTS.md
2. ES_CONFORMANCE.md
3. Память: `session_2026_10_15_rust199_boxing_sticky.md` и
   `project_round3_whole_engine_scope.md`
4. `PROMPT_HARD_REFACTOR_4.md`: только разделы «Метрика», «Жёсткие правила»
   и «Гейт». Описание WIP там устарело.

## Текущее состояние

- main, HEAD `8029082b0`. Tracked-дерево чистое.
- Тулчейн: rustc 1.99 stable. Переезд закоммичен в `04cf52823`.
- Последние коммиты (все G):
  - `f965e9283`: упаковка Float64 через FJCVTZS.
  - `8029082b0`: якорный sticky-поиск в regex.
- Baseline-бинарь для замеров:
  `benchmarks/results/regexp-anchored-search/otter-gate`, SHA256 `8d727a91…`.
- База Test262: `benchmarks/results/t262/full-2026-10-15e.json`,
  53575 / 52463 pass / 12 fail / 1100 skip, 0 timeout.
- Скрипты гейта для переиспользования лежат в
  `benchmarks/results/regexp-anchored-search/`: `run_gate.py` (с фазой
  `fixup`), `run_observations.py`, `run_checked_build.py`,
  `run_checked_gc.py`, `validate_test262.py`, `make_reports.py`,
  `measure.py`. Копируй их в каталог нового слайса и меняй пути, имя
  отчёта Test262 и базу.

## Запрет на этот раунд

RegExp не трогать: ни движок, ни exec/replace/split. Прошлые сессии
слишком долго в нём сидели. Исключение — регрессия, которую вскрыл гейт.

## Порядок работы

1. Проверить, нет ли зависших процессов otter. Затем снять wall-матрицу:
   `scripts/dev/wallclock_matrix.sh <otter-gate> <out> 5` против node и bun.
   Ранжировать бенчмарки по разрыву с node, исключив regexp.
   Диагностические профили по видам узлов —
   `scripts/dev/sample_kind_prof.py`. Доли из профиля не выдавать за
   выигрыш wall-clock.
2. Основная цель — стоимость доступа к свойствам и проверки формы
   («shape в заголовке»):
   - Сейчас `CheckShapes` в graph-тире ARM64 занимает около 10 инструкций:
     `tst`-тег + `cbz` + `ldrb` type-байт + `cmp` + `ldr` shape по
     смещению 8 + (`sub`) + `cmp`. У node — 3–4 инструкции.
     См. `emit_check_shapes` / `emit_object_receiver` в
     `crates/otter-jit/src/graph/arm64.rs` и аналоги для x86.
   - Причина: 8-байтный GC-заголовок `type:u8 | flags:u8 | site:u16 |
     size:u32`, а shape лежит только у объектных тел, по смещению 8.
     У не-объектов на этом смещении сырые данные, поэтому без проверки
     типа сравнивать shape нельзя.
   - Сначала изучить, как это сделано у других движков: map-слово первым
     словом каждого HeapObject; StructureID в заголовке ячейки. Портировать
     архитектуру, а не придумывать эвристику.
   - Цель: проверка формы = одна загрузка + одно сравнение (+ тег-тест
     для неизвестного значения). Все типы ячеек должны иметь
     идентификатор формы/типа в одном фиксированном слове, которое никогда
     не совпадает с shape объекта.
   - Что придётся менять: заголовок в `otter-gc`; форвардинг
     скавенджера (смещение форварда лежит в первых 8 байтах после
     заголовка); аллокационные стабы обоих тиров; snapshot/heap image;
     эмиттеры template, graph arm64 и x86; `LoadArrayLength` (~10
     инструкций). Внутренний контракт менять напрямую, без слоёв
     совместимости.
   - Перед кодом дочитать полностью `crates/otter-gc/src/header.rs`,
     `scavenger.rs` и места, где читаются `object_shape_byte` и
     `OBJECT_BODY_TYPE_TAG`.
3. Если shape-в-заголовке окажется слишком большим для одного слайса,
   разбить его на проверяемые вертикальные шаги. Каждый шаг закрывать
   полным гейтом и отдельным коммитом. Ни один шаг не должен
   регрессировать.
4. Запасные цели по матрице (тоже не regexp):
   - deltablue / richards: константные поля константных объектов,
     single-closure зависимость собственного контекста;
   - native-call glue (`call_generic_entry`);
   - GC в splay.

## Гейт — быстрее, чем в прошлый раз

- Пока идёт гейт, НЕ менять ни один файл из freeze, даже тесты:
  checked GC в конце сверяет freeze и упадёт.
- Перед гейтом прогнать узкие проверки, чтобы гейт не падал на
  тривиальном:
  - clippy host;
  - `cargo test` затронутых крейтов;
  - один-два затронутых `--test`;
  - сравнение нового корпуса с Node во всех режимах и под stress1;
  - x86 clippy и x86-тесты затронутых крейтов.
- Порядок и параллельность фаз:
  - сначала host, т. к. он собирает финальный release CLI;
  - затем параллельно: (a) test262 → difftest-host → x86;
    (b) checked build → checked GC; (c) oracle/gc/gcfull на финальном CLI;
  - замеры — только после окончания гейта, на тихой машине.
- Состав гейта:
  - clippy host и x86;
  - VM lib;
  - JIT lib arm64 и x86;
  - runtime: `jit_speculative_property_loads`, `code_chunk_eviction`,
    `jit_native_context_closure`, `jit_boxed_arithmetic` — host и x86;
  - regex lib host и x86;
  - difftest host и x86;
  - полный Test262 одним runner'ом, новый уникальный отчёт, база 15e;
  - oracle, gc, gcfull;
  - checked GC (все корпуса × 16 страйдов × 3 тира).
  - Новый difftest-корпус слайса добавить в oracle и gcfull.

## Критичные правила

- Только main. Без worktree, stash, checkout, reset. Чужие untracked-файлы
  не трогать.
- Без новых env/feature-флагов, `thread_local`, `static mut`.
- GC roots, deopt, исключения и порядок эффектов сохранять.
- rustfmt только для leaf-файлов; для владельцев модулей — `--check`.
- Не запускать `cargo test --workspace` и пачки `--test`.
- Главная метрика — production wall-clock: 3×5 balanced
  parent/current/Node. Инструкции — только диагностика.
- Неудачные попытки и ограничения измерений сохранять, не скрывать.
- Коммиты: GPG-подпись, проверить `G`, без AI/Co-Authored-By trailers.
  Коммитить отдельным вызовом, после просмотра результатов.
- Из результатов в git идут только `summary.md` (по-русски),
  `gate-summary.json` и `performance.json`, добавляются через `git add -f`.
- После коммита:
  - `rm -rf target/x86_64-apple-darwin`;
  - старый baseline удалить, новый сохранить;
  - на macOS перед копированием поверх бинаря сначала удалять
    destination.
- Для нового или существенно изменённого Rust-модуля — точные `//!` docs.

Не останавливайся на плане: реализуй, прогони гейт, замерь, закоммить и
переходи к следующему шагу.
