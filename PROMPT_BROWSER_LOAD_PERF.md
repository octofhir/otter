Продолжи разработку движка Otter автономно в репозитории
/Users/alexanderstreltsov/work/octofhir/otter.

Работай как инженер, который строит JIT-компиляторы и VM уровня V8 / JSC /
SpiderMonkey. Пользователь не участвует: не задавай вопросов и не
останавливайся после одного среза.

## Цель сессии: загрузка реальных страниц в браузере Otlyra

Otter — JS-движок браузера Otlyra (`../Otlyra`). Тёплая загрузка страницы
у Otlyra заканчивается в 2–6 раз позже, чем у Chrome, и на JS-тяжёлых
сайтах **половина и больше CPU загрузки — Otter**. Результат сессии —
меньше миллисекунд Otter на загрузку реальных страниц. Микроядра и Octane
здесь не цель; каждый срез обязан сдвинуть время загрузки хотя бы одной
страницы или время компиляции/исполнения её реальных бандлов, с цифрами
до/после в отчёте и в сообщении коммита.

### Как это измерено (2026-10-17, Apple M-series, тёплые кэши)

Профиль Time Profiler всей загрузки страницы в Otlyra (`--screenshot`),
доля сэмплов главного потока по крейтам:

| Сайт (CPU загрузки) | Otter | из них | stylo | текст | вёрстка |
|---|---|---|---|---|---|
| The Verge (5.5 с) | 61% | vm 22%, GC 14%, компилятор 9%, интерп. 7%, JIT 6%, парсер 3% | 5.5% | 7% | 1.5% |
| GitHub (3.2 с) | 49% | компилятор 17%, vm 14%, парсер 9% | 16% | 10% | 2% |
| BBC (2.9 с) | 54% | vm 25%, интерп. 10%, компилятор 7.5%, JIT 7% | 3% | 15% | 3% |
| YouTube (3.4 с) | 47% | vm 18%, компилятор 16%, JIT 5%, парсер 5% | 20% | 0% | 0% |
| Apple (2.2 с) | 17% | | 27% | 23% | 6% |

Chrome на тех же страницах (тёплый кэш): FCP 80–480 мс, `load` 250–1500 мс.

Уже сделано (не повторять):
- `5d4a17350` — census кода больше не запускается на каждом ходу, когда
  живой eval/import-код выше high-water mark (было 650 мс на The Verge:
  полный GC + обход всей кучи на каждый `run_script`).
- `bda926bac` — `RuntimeBuilder::execution_stats(false)`: результаты без
  статистики (обход IC всех функций на каждый ход, ~90 мс на YouTube).
  Otlyra его уже включила.

### Как браузер пользуется движком (важно для приоритетов)

- Каждый `<script>` — `Runtime::run_script(SourceInput, url)`; модули —
  через `WebModuleMap` + `scan_module_requests`, затем линковка графа.
- **Каждое событие, таймер, rAF, MutationObserver, custom-element reaction
  — тоже `run_script` маленькой строки** вида `__otlyraFlushDeferred(true)`
  или `__otlyraNotifyMutationObservers()`: парсинг + компиляция + линковка
  нового чанка на каждый вызов, в кучу размером с SPA. Сотни-тысячи ходов
  за загрузку. Всё, что стоит O(весь код) или O(вся куча) на ход, —
  убийственно.
- Бандлы — минифицированные однострочные файлы по 0.5–4 МБ.

## Приоритеты (в порядке ожидаемого выигрыша)

1. **Ленивая компиляция функций.** Сейчас компилируется всё сразу:
   GitHub — 830 мс в парсере+компиляторе (672 мс сам `otter_compiler`),
   The Verge — 580 мс. V8 предразбирает внутренние функции (только
   проверка синтаксиса и сбор ранних ошибок/контекстов захвата) и
   компилирует тело при первом вызове; большая часть кода страницы не
   вызывается никогда. Сделай так же: скрытые классы/IC/JIT не должны
   страдать, ранние SyntaxError — по спеку (ошибки, которые обязаны
   всплыть при разборе всего скрипта, всплывают сразу).
2. **Пропускная способность компилятора.** В профиле горячие:
   несколько отдельных обходов AST (`strict_validation` ×2, `capture::Builder`,
   `ModuleMetadata`), `FunctionContext::finish_code`, `Compiler::resolve_name`,
   `ModuleBuilder::intern_constant`, `std::hash` SipHash (`RandomState`) в
   компиляторе, много мелких `malloc/free`/`memmove`, `Vec<Operand>` на
   каждый emit, `SpanEntry`-расширения. Сливай обходы, арены вместо
   россыпи `Vec`, FxHash, меньше копий.
3. **Кэш байткода для встраивающего кода.** Chrome на тёплой загрузке берёт
   байткод из code cache и почти не компилирует. Дай API: скомпилировать
   скрипт/модуль в байты (проверяемые при загрузке: версия движка, хеш
   исходника, верификатор) и запустить из этих байтов; браузер положит их
   в свой дисковый кэш рядом с ответом. Байткод уже сериализуется
   (`otter-bytecode`, `link_verified_module`, `build.rs` Otlyra компилирует
   свой glue в байткод на этапе сборки) — переиспользуй.
4. **Вызов функции из встраивающего кода без компиляции строки.** Публичного
   API «вызвать persistent-корень-функцию с this и аргументами» в
   `otter-runtime` нет (`install_native_global_call` — не то). Добавь
   `Runtime::call(...)` с теми же гарантиями хода, что у `run_script`
   (microtask checkpoint, uncaught-ошибки, watchdog-прерывание), чтобы
   браузер перевёл все диспатчи на него.
5. **Пер-ходовые расходы, пропорциональные всему коду:**
   - `JitCodeRegistry::take_generated_feedback` при каждом выходе из
     внешней нативной активации обходит **все** entry cells (64 мс на
     The Verge). Нужен список ячеек, в которые реально входили.
   - `ExecutableModule::function_slot` из `begin_suspendable_activation`,
     `CodeSpace::resolve_chunk` из JIT-стабов (`RuntimeCall::bind`) —
     поиск чанка/слота на горячем пути вызова.
   - Оставшийся census (`reclaim_dynamic_code`, ещё ~190 мс на The Verge
     при росте кода): дешевле перепись или инкрементально.
6. **VM-горячие точки** («vm other», 1.2 с на The Verge): посмотри
   свежим профилем; там были `shape_offset_of_str`,
   `ordinary_set_data_value`, `ModuleSource::line_col` при захвате стека
   ошибок (на однострочном 2-мегабайтном бандле колонка в UTF-16 считается
   проходом от начала строки — кэшируй), `FeedbackVector` drop, GC census.
7. **GC** (352 мс на The Verge после фикса census): проверь, что
   молодое поколение не продвигает весь граф DOM-обёрток, и что нет
   других принудительных полных сборок между ходами.

## Как мерить

Сборка Otlyra идёт с `[patch]` на `../otter`, так что изменения движка
сразу попадают в браузер.

- Быстрый цикл без браузера: сохрани реальные бандлы
  (`curl -A 'Mozilla/5.0 … Chrome/141' https://github.com` → `<script
  src>`/`modulepreload` с github.githubassets.com; The Verge, YouTube) в
  `benchmarks/fixtures/web-bundles/` и сделай бенчмарк в
  `otter-benchmark`: (а) разбор+компиляция каждого бандла, (б) «браузерный»
  паттерн — загрузить бандлы, затем 2000 ходов `run_script` маленькой
  строки, вызывающей функцию в большой куче, (в) то же с >16 МиБ живого
  eval-кода. Это регрессионная база для приоритетов 1–5.
- Конечная метрика — Otlyra (только с `OTLYRA_NO_KEYCHAIN=1`):
  ```
  cd ../Otlyra && cargo build --release -p otlyra-app
  OTLYRA_NO_KEYCHAIN=1 OTLYRA_LOG="warn,page.timing=info" target/release/otlyra \
    --url https://www.theverge.com --screenshot /tmp/x.png --no-interface \
    --width 1280 --height 900 --scale-factor 1
  ```
  Строка `page.timing` даёт стадии загрузки (`scripts_ms`, `load_event_ms`,
  `settle_ms`, `total_ms`). Профиль:
  ```
  xcrun xctrace record --template 'Time Profiler' --output v.trace \
    --env OTLYRA_NO_KEYCHAIN=1 --launch -- target/release/otlyra --url … --screenshot …
  xcrun xctrace export --input v.trace \
    --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' --output tp.xml
  python3 tools/profile-split.py tp.xml            # доли по крейтам
  python3 tools/profile-inclusive.py tp.xml 'otter_compiler' 'otter_gc'
  python3 tools/profile-callers.py tp.xml 'take_generated_feedback' 8
  python3 tools/chrome-load.py 3 https://github.com   # Chrome для сравнения
  ```
- Сайты: github.com, theverge.com, bbc.com, youtube.com, apple.com. Каждую
  цифру — минимум 2 прогона, бинарники до/после вперемешку. Машина должна
  быть простаивающей (сборки и агенты завышают в ~3 раза) — если нет,
  пиши об этом рядом с цифрой.

## Правила

- Старт: `date`, `git status`, `git log -3`. Работай на `main`, локальные
  коммиты, без веток, без push. Автор:
  `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com"`,
  **без Co-Authored-By**.
- Каждое изменение Otter — с тестом в Otter (регрессионный тест на
  поведение и, где это перф-механизм, тест на сам механизм: число проходов,
  отсутствие работы и т.п.).
- Не запускай весь набор тестов `otter-runtime` (не помещается на диск):
  только выбранные `--test`, флаги через bash-массив
  (`bash -c 'args=(); for t in a b; do args+=(--test "$t"); done; cargo test -p otter-runtime "${args[@]}"'`),
  zsh не разбивает `$T` и соберёт все 200+ тестовых бинарников. Проверяй,
  что строки `test result` показывают реально прошедшие тесты.
- Диск: `df -h /System/Volumes/Data` перед каждой сборкой; ниже 15 ГБ —
  удали `target/debug/incremental` и устаревшие артефакты своих пакетов.
  Никогда не трогай `target/release` движка, кэш Skia и артефакты замеров.
  Без worktrees и лишних target-директорий.
- Не делай `git checkout/reset/stash` по грязным файлам; временные правки
  откатывай из своей копии или `git show HEAD:path > path`.
- Известные красные тесты не твои: `shared_runtime_host`,
  `jit_exception_regions::stack_owned_nested_catch…`, `jit_module_ops`.
- fmt/clippy (`-D warnings`) на изменённых пакетах перед каждым коммитом;
  в `otter-gc/src/heap.rs` есть чужое расхождение fmt — не трогай.
- Если изменение API движка требует правки Otlyra (например, перевод
  диспатчей на `Runtime::call` или кэш байткода), сделай API и тест в
  Otter, а в отчёте опиши, что и где поменять в Otlyra
  (`crates/otlyra-script/src/host.rs`, `page.rs`).

## Отчёт в конце (по-русски)

Коммиты; таблица «до/после» по каждому сайту (`page.timing` и доли Otter
из профиля) и по бенчмарку бандлов; что не сделано и почему; что нужно
поменять на стороне Otlyra, чтобы получить выигрыш.
