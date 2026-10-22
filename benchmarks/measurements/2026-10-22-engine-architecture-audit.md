# Архитектурный аудит Otter и план крупных рефакторов

## Цель и критерий

Цель — конкурентная скорость полного исполнения, прогретого исполнения, startup и компиляции при минимальных peak RSS и удерживаемой памяти. Лидерство над другими движками должно подтверждаться на опубликованной матрице, а не одним TS-прогоном или снижением числа инструкций. Превосходство над всеми движками на любых программах не является проверяемым обещанием.

Работа идёт в текущем checkout. Все исходные изменения сохранены одним checkpoint-коммитом `fa4cc6668`. Последующий field-observer эксперимент не принят как достижение цели: сокращение emitted code около 17% дало только 2,7% медианного выигрыша полного ts, без убедительного уменьшения RSS. Его executable, patch и пять пар сохранены в `benchmarks/results/metadata-ownership-2026-10-21/compact-experiment/`. Observer-прототип полностью удалён из активного кода; его последний patch сохранён в `benchmarks/results/architecture-census-2026-10-22/rejected-observer.patch`.

## Следующий срез: наблюдения JIT перестают владеть JS graph

Baseline этого среза — `4c8dff094`, executable `shared-wordcode-2026-10-22/final/after-otter`, SHA256 `eb833505405f36ea230810818d26e09a01f3d07c23231700b72fd2ff7b108dcc`. Аудит выполнялся параллельно по трём направлениям: реальные GC owners, горячий Graph и source/compiler metadata.

### Выбор по подтверждённой стоимости

| Кандидат | Подтверждённая причина / граница | Решение |
|---|---|---|
| Время жизни speculative JS observations | Возвратившийся callee с массивом 1 048 576 элементов после двух GC удерживает 9 572 840 Б heap в production против 1 052 856 Б interpreter. Отдельно первая closure context без native compilation удерживает 9 656 560 Б даже в interpreter | Выбран: устранить сильное владение из writable identity caches и first-context observation; сохранить сильные native constants |
| Object/field SSA и sinking | В самых горячих Box2D bodies сотни field reads/conversions и почти нет allocations. В DeltaBlue `chainTest` есть receivers, но реальные CallJs, context stores и loop states блокируют sinking | Следующий execution срез должен менять field/alias/loop model, а не повторять отвергнутый global Float64 boxing |
| Cold source facts | CodeBlock копирует +4 Б/op byte-PC mapping, compressed spans разворачиваются в 12-байтовые записи; lazy environment и preparse forest могут копироваться по глубине | Величина реального lazy/preparse удержания ещё не измерена. Не выбран ещё один небольшой source-only рефактор |

### Единственная модель владения

* `CompilationRoot` удерживает Rust carrier на срок pending compilation / установленного native generation. Само существование carrier не означает владения наблюдавшимся JS значением.
* Writable callee identity и пара target/prototype `instanceof` — слабые наблюдения. Живые значения переписываются после nursery movement; мёртвый callee становится EMPTY, а потеря любого члена пары очищает всю пару. Следующий native hit проходит обычную исходную proof. Новый runtime call, replay и дополнительный hit guard не добавлены.
* `ClosureContexts` владеет address-stable фактом источника, но первая наблюдённая context слаба. Смерть context навсегда переводит факт в unavailable до повторного использования cage offset: потеря первого наблюдения не разрешает считать следующий контекст первым.
* Когда compiler действительно специализирует body по этой context, он сначала публикует сильную held cell в текущей compile session. Именно её адрес получает `unique_closure_context`. Pending result и затем native generation удерживают эту context до физического retirement. Неиспользованная cell удаляется, если constants отсутствуют. Cell-values методов и context slots также остаются сильными: native `LoadCellValue` читает их без локальной проверки.
* Один `CalleeIdentityCell` carrier обслуживает оба домена; private strength bit отделяет immutable constant от writable cache. Цена — 8 дополнительных байт Rust payload на таком carrier на текущем host и отдельная held cell у реально специализируемой unique context. Это не новый registry или второй GC entry.

GC получает один `ExtraRootSource::process_weak` протокол. Nursery вызывает его после strong tracing и ephemeron fixpoint, пока forwarding headers ещё существуют, до flip и освобождения young-large. Major вызывает его непосредственно перед sweep после окончательного marking, включая публичный split incremental mark/sweep путь. Composite native scopes передают hook; повторная обработка безопасна. Resolver никогда не маркирует и не эвакуирует целевые объекты. GC plumbing хранит raw boundary, VM обновляет atomic Value words и scalar dependency flags без managed allocations или JS reentry.

Существующее условие оптимизатора «GC не ломает активные assumptions» сохранено: generation-owned context и все unguarded constants сильны. Поэтому не понадобилось ослаблять `LoadCellValue` effects, добавлять постфактум deopt после allocation или менять hit-код одной ISA в отрыве от другой. ARM64 и x86_64 используют тот же адрес сильной cell и тот же EMPTY miss у слабого cache.

### Сопоставление с исходниками других движков

[V8: weak objects in optimized code](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/code-inl.h) различает weak JSReceiver/Context/PropertyCell references и обязательные materialization maps. В [major collector](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/heap/mark-compact.cc) смерть embedded target инвалидирует dependent code. Otter пока не переносит эту более широкую weak-code модель: для direct constants она требует согласованной invalidation и проверки на collecting boundaries.

[JSC CodeBlock liveness](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/CodeBlock.cpp) отдельно доказывает независимую достижимость weak dependencies неактивного optimized body; marked/active CodeBlock может удерживать необходимые ссылки сильными. [Pending DFG references](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGDesiredWeakReferences.cpp) трассируются сильно во время compilation. Применённое разделение observation / pending-and-installed assumption следует этой границе времени жизни, без копирования чужого collector или code liveness protocol.

### Проверка и измерения среза

Сырые данные, команды, binary/source identities и оба минимальных retention reproducer: `benchmarks/results/root-retention-2026-10-22/`.

Пять чередующихся AB/BA-пар после исключённой warmup пары, полный неизменённый fixture, отдельные процессы. Все 36 запусков завершились с одинаковым результатом. ARM64 M1 / 16 ГиБ / macOS 26.5, production opt3/thin LTO/codegen1/debug1/default features; RUSTFLAGS и CARGO_TARGET_DIR отсутствовали. Параллельных Rust builds и profiler не было; обычные фоновые host processes сохранены в snapshots. After binary SHA256 `0a4e85804f81e7e33169b66fc14cfde725424aad810a05b9d11d084e6a73f4c7`.

| Класс | Wall median, с до → после | Δ wall | CPU median, с до → после | Peak RSS median, МиБ до → после | Δ RSS |
|---|---:|---:|---:|---:|---:|
| deltablue | 0.6658 → 0.6804 | +2.19% | 0.69 → 0.71 | 60.20 → 62.77 | +4.26% |
| splay | 0.6655 → 0.6912 | +3.86% | 1.23 → 1.24 | 375.06 → 379.89 | +1.29% |
| ts | 3.2537 → 3.5745 | +9.86% | 4.49 → 4.49 | 563.22 → 500.00 | -11.22% |

| Класс | Wall min…max до / после, с | Peak RSS min…max до / после, МиБ |
|---|---:|---:|
| deltablue | 0.6517…0.6776 / 0.6621…0.7200 | 58.73…61.72 / 58.38…65.39 |
| splay | 0.6599…0.6935 / 0.6734…0.6989 | 365.31…383.88 / 373.80…383.70 |
| ts | 3.0987…4.0171 / 3.0491…4.5962 | 526.98…623.08 / 492.55…504.98 |

Три post-GC AB/BA-пары, отдельные процессы. Для TS это require-wrapper, а не plain fixture вход выше.

| Сценарий | Live heap median до → после, МиБ | Heap min…max до / после, МиБ | Post-GC RSS median до → после, МиБ |
|---|---:|---:|---:|
| callee | 9.13 → 1.00 | 9.13…9.13 / 1.00…1.00 | 36.11 → 28.06 |
| source | 9.21 → 1.08 | 9.21…9.21 / 1.08…1.08 | 32.25 → 24.22 |
| ts | 254.41 → 7.88 | 254.09…295.22 / 7.88…48.80 | 552.78 → 309.64 |

Постоянное владение JS graph из observations устранено: TS median live heap −96,90%, post-GC RSS −43,99%, обычный median peak RSS −11,23%. Это подтверждённое крупное улучшение удержания памяти. **Ускорение полного исполнения не достигнуто**: TS CPU median одинаков, wall median хуже на 9,86%, DeltaBlue/Splay medians тоже хуже. Разброс TS велик и интервалы перекрываются; ни регрессию, ни отсутствие регрессии нельзя заменять выбором самого быстрого запуска. В одной из трёх after retention пар TS сохраняет 48,80 МиБ вместо 7,88: остаточный шаг порядка 41 МиБ не объявлен локализованным или устранённым.

Проверки: GC extra-root filter 4; VM identity owners 2 и source-context 2; настоящие native context/closure cases по 2 на ARM64 и x86_64/Rosetta; x86 owner tests 2; один hot-eval eviction regression; новый JS case и existing unique-context case во всех трёх tier policies против baseline interpreter; новый case GC strides 1/4/16; семь реальных Graph bodies в artifacts. Всего diagnostic driver записал 30 успешных процессов. Форматирование проверено на изменённых Rust files; incidental baseline-only formatting из `cargo fmt --all` удалено. Полные suites/Clippy/Test262 не запускались. `compiled-source.patch`, `build-manifest.json`, `build-commands.json`, `diagnostics.json`, `whole-ab/results.json`, host/process snapshots и исходные stdout/stderr сохранены.

Следующий execution срез: Graph ArrayPush/ArrayPop сейчас имеют World effects из-за committed arbitrary-JS protocol miss. Уже существующий ARRAY_PUSH_ALLOC — no-JS collecting Probe с отказом до append/length mutation; ARRAY_POP_LEAF тоже отказывает до mutation. Перевести оба native backend на эти окончательные границы, сохранить native fits и точное eager recovery, сузить effects до array storage/length/elements + Movement. Проверять не только стоимость самого push, но сохранение чужих field/shape/context facts через массивную операцию. Требуются безопасная staging трёх ABI operands, GC внутри growth, прототип/accessor/readonly/COW/widening случаи и full A/B.


Остаточные сильные владельцы: реальные generation constants, shape → prototype graph, активные frames и pending compilation. Их удержание не объявляется исправленным этим срезом. `process.memoryUsage().external/arrayBuffers == 0` остаётся неполным API coverage, а не доказательством отсутствия внешней памяти.

## Результат законченного shared-wordcode среза

Удалён второй VM instruction/overflow массив; compiler, CodeBlock и native snapshots читают один immutable body. Одновременно удерживаемые compiler+VM instruction данные изменились с 56 до 28 байт/op, включая новую IC directory. Есть отдельная цена owner header у холодных функций. Исправлен baseline crash nested merge phi; numeric, feedback-arena и liveness прототипы в финальный код не входят.

Пять чередующихся AB/BA-пар после одной исключённой warmup пары на каждом полном неизменённом fixture. Apple M1, 16 ГиБ, macOS 26.5; оба CLI отдельно собраны `cargo build --release -p otter-cli`, default features, opt 3/thin LTO/codegen-units 1/debug 1, без RUSTFLAGS. CPU — медиана user+sys одного процесса со всеми его потоками. Source и binary SHA, команды, watchdog, stdout/stderr и process snapshots сохранены. Во всех 96 запусках, включая warmup, результаты совпали.

| Класс | Wall, с: до → после | Δ | CPU, с: до → после | Peak RSS, МиБ: до → после | Δ RSS |
|---|---:|---:|---:|---:|---:|
| crypto | 0.2074 → 0.2082 | +0.39% | 0.21 → 0.21 | 43.80 → 43.58 | -0.50% |
| deltablue | 0.6643 → 0.6548 | -1.44% | 0.69 → 0.68 | 60.55 → 59.19 | -2.25% |
| box2d | 0.6280 → 0.6274 | -0.08% | 0.72 → 0.72 | 133.25 → 133.47 | +0.16% |
| raytrace | 0.8237 → 0.8264 | +0.34% | 0.83 → 0.83 | 47.00 → 47.00 | +0.00% |
| splay | 0.6649 → 0.6573 | -1.14% | 1.22 → 1.20 | 367.36 → 376.58 | +2.51% |
| regexp | 1.3451 → 1.3243 | -1.54% | 1.36 → 1.34 | 140.39 → 139.62 | -0.55% |
| mandreel | 1.0014 → 1.0149 | +1.35% | 1.06 → 1.08 | 222.81 → 222.94 | +0.06% |
| ts | 3.3022 → 3.1547 | -4.47% | 4.34 → 4.29 | 609.08 → 572.75 | -5.96% |

Разброс пяти измеренных запусков:

| Класс | Wall до min…max, с | Wall после min…max, с | Парные Δ wall min…max |
|---|---:|---:|---:|
| crypto | 0.2064…0.2254 | 0.2068…0.2172 | -3.61…+0.89% |
| deltablue | 0.6504…0.6797 | 0.6464…0.6737 | -2.81…+1.41% |
| box2d | 0.6173…0.6458 | 0.6216…0.6359 | -2.85…+3.02% |
| raytrace | 0.7926…0.8598 | 0.7982…0.8738 | -4.88…+10.25% |
| splay | 0.6640…0.7074 | 0.6446…0.6674 | -8.88…+0.50% |
| regexp | 1.3339…1.3590 | 1.3122…1.3559 | -3.44…+1.50% |
| mandreel | 0.9991…1.0134 | 1.0053…1.0329 | +0.12…+3.38% |
| ts | 3.1803…3.3489 | 3.1475…3.6575 | -5.06…+9.21% |

Медианные изменения основного исполнения остаются небольшими; интервалы перекрываются. Значимого общего ускорения и выполнения цели «лучший движок» эти результаты не доказывают. Rust-сборка, profiler и diagnostic probes не работали параллельно с A/B. Обычные фоновые host процессы сохранялись в process snapshots; host не был изолированной benchmark-машиной. Холодная подготовка, JIT compilation и JS execution включены в полное process wall/CPU; их отдельное ускорение здесь не измерялось.

Отдельный retention сценарий: require полного fixture, затем два явных GC и `process.memoryUsage()`, три AB/BA-пары. Это активный CLI wrapper, отличный от plain fixture входа выше; его данные не подставляются вместо peak RSS. `heapUsed` — GC accounting, не Rust/JIT memory и не committed space.

| Класс | Post-GC RSS median до → после, МиБ | RSS min…max до / после | Live JS heap median до → после, МиБ | Heap min…max до / после |
|---|---:|---:|---:|---:|
| mandreel | 218.58 → 204.78 | 216.81…218.77 / 204.50…221.27 | 1.22 → 1.22 | 1.22…1.22 / 1.22…1.22 |
| box2d | 107.91 → 108.03 | 106.08…109.36 / 107.59…109.98 | 3.02 → 3.02 | 3.01…3.02 / 3.02…3.02 |
| ts | 507.22 → 557.00 | 492.77…526.16 / 545.77…576.55 | 213.28 → 254.09 | 212.95…254.09 / 254.09…295.22 |

Mandreel даёт адресное подтверждение меньшего retained Rust state при одинаковом live JS heap: медиана RSS ниже на 13,80 МиБ (−6,31%). В TS wrapper после GC RSS и JS heap выросли, причём шаги порядка 41 МиБ встречались у baseline и предыдущих прототипов. Причина удержания JS graph не установлена; экономией wordcode её закрывать нельзя. Этот retention пункт остаётся открытым и не позволяет объявить общую memory/performance цель достигнутой.

Холодный embedded census финального executable после `Runtime::run_script` и forced full GC: SourceModuleBytes Mandreel 65,98 МиБ, Box2D 5,09 МиБ, TS 21,49 МиБ. Старый diagnostic census давал Mandreel 80,15 МиБ, но включал observer-прототип: это компонентное подтверждение геометрии, не чистый performance A/B. В этих трёх финальных census после drop runtime все resource classes вернулись к нулю. Census ledger не покрывает весь allocator/system RSS и не суммируется с GC cells.

Фактически выполнено: wordcode/serde и retained geometry filters; один shared-owner/charge regression с detach и budget refusal; VM executable, resource/snapshot и feedback filters; native Graph/source-map/property/return-site filters на ARM64 (26) и x86_64/Rosetta (28); финальный x86_64 check; оба адресных JS случая во всех трёх режимах против checkpoint interpreter; moving GC strides 1/4/16 на shared-wordcode case; native artifact capture с шестью Graph bodies. Нового unsafe, GC root recipe, value boxing, runtime mode или format version нет.

Финальные сырые результаты: `benchmarks/results/shared-wordcode-2026-10-22/final/`. Предварительная полная матрица до cleanup allocation-error handling сохранена отдельно в родительской директории; основной таблицей является только финальная матрица выше. Неудачные промежуточные проверки и их исправления также сохранены, без подмены первого результата последним.

## Что уже сделано в архитектуре

* Обычный `ObjectBody` — 16 байт фиксированного payload плюс GC header и trailing fields. Это три compressed handles: shape, overflow slab и rare/exotic sidecar. Переписывать его как якобы 96-байтовый объект из старого сентябрьского отчёта было бы ошибкой.
* Prototype и семантическое состояние принадлежат shape. Отдельный prototype pointer на каждом объекте уже устранён.
* `CodeBlock` уже разделяется через `Arc`; инструкции не копируются в каждый `JitCompileSnapshot`. Captured bindings уже лежат в scope contexts, а не в отдельных old-space binding cells.
* Базовый `InstructionFeedback` — 4 байта на инструкцию. Это не размер всего feedback: отдельно существуют typed slots и eagerly allocated property/call payloads.
* Native generations имеют единый registry, physical-retirement epoch, accounting и постоянные entry-cell tombstones. Нельзя объявлять всякое retired entry удерживаемым executable: mapping мог уже освободиться.

Основные владельцы: `CodeSpace::ChunkPayload` удерживает linked bytecode, executable view, atom table и source registry; `CodeBlock` — горячую запись, feedback и CFG; transient compile snapshot — замороженные proof/call/inline планы; `Graph` — SSA и frame states; установленный code object/registry — mapping, deopt/safepoints и compilation roots; GC heap — managed cells и пространства. Это действующий стек, второй runtime не нужен.

## Фактический census по классам нагрузки

Один диагностический запуск неизменённого исходника на класс: direct embedded `Runtime`, production tier policy, без CLI startup image. Это разбор владения, не A/B-результат скорости и не сравнение с Node/Bun. Bootstrap, завершение исходника, forced full GC и drop runtime сняты отдельно. Скрипты завершились успешно. После GC счётчики live cells reconciled; до GC per-type строки не включают полную историю generated allocations. Ledger классы перекрываются с GC accounting; их нельзя складывать в RSS. Нулевые heap/external ledger counters не означают нулевое потребление этих категорий.

| Нагрузка | Allocation traffic, МиБ | Minor/full во время скрипта | RSS completion → после GC, МиБ | Live GC cells после GC, МиБ | Source/module ledger, МиБ | Generated code + metadata ledger, МиБ |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| empty | 0 | 0/0 | 15,73 → 15,92 | 0,46 | ≈0 | 0 |
| crypto | 6,44 | 1/0 | 33,30 → 33,08 | 0,54 | 1,03 | 1,63 |
| deltablue | 836,18 | 209/0 | 46,69 → 46,50 | 0,52 | 0,69 | 2,13 |
| box2d | 75,39 | 8/0 | 112,44 → 89,02 | 2,35 | 5,57 | 8,82 |
| splay | 259,51 | 13/3 | 339,84 → 84,67 | 0,55 | 0,30 | 1,07 |
| regexp | 846,34 | 98/0 | 129,52 → 92,28 | 3,31 | 2,05 | 4,61 |
| mandreel | 1,03 | 0/0 | 233,52 → 233,94 | 0,87 | 80,15 | 16,50 |

Эти значения относятся к сохранённому census executable и его точному patch. В него вошёл рабочий observer experiment; это не checkpoint baseline для будущего A/B. Остаток RSS нельзя автоматически назвать утечкой: там есть allocator retention, compiler transient buffers, stacks, mappings и неохваченные ledger bookkeeping. Source/module ledger тоже требует разбивки по конкретным физическим владельцам.

Дополнительные CLI-профили checkpoint, по одному запуску без Rust-сборки:

| Нагрузка | Graph, % isolate samples | VM/runtime | Frontend | GC |
| --- | ---: | ---: | ---: | ---: |
| deltablue | 71,6 | 11,9 | 0,6 | 3,0 |
| box2d | 63,5 | 14,9 | 1,4 | 2,2 |
| mandreel | 56,2 | 6,9 | 13,8 | 0,8 |

Это sample shares, не доли retired instructions. Compiler worker — отдельный поток; isolate share не равен суммарному времени JIT compilation.

Offset/artifact разбор двух workload: в Box2D `CheckedTaggedToFloat64` занимает 13,6% Graph samples, `Float64ToTagged` 5,4%, `LoadOwnField` 13,6%, `CheckShapes` 6,3%. В DeltaBlue распределение иное: Branch 11,2%, ArrayPush 8,1%, LoadOwnField 7,9%, StoreNamedProperty 7,3%, LoadPresentElement 6,9%. На основании этого нельзя лечить оба одной store-probe настройкой.

## Что показывает сборка

Сохранённый `cargo build --release -p otter-cli --timings` пересобрал затронутую цепочку за 3 мин 14 с: CLI 101,24 с, VM 90,91 с, runtime 42,01 с, JIT 36,64 с. Это перекрывающиеся unit durations инкрементальной сборки, их нельзя складывать или выдавать за clean-build стоимость. Production profile: opt-level 3, thin LTO, codegen-units 1, debug 1. Редактирование VM затрагивает JIT/runtime/product crates; поэтому release CLI собирается после цельного среза, отдельно от тестовых и benchmark features.

`features.txt` и `dependency-duplicates.txt` показывают ICU, Wasmtime/Cranelift в product graph, wasm-encoder 0.251/0.253, syn 2/3 и compact_str 0.9/0.10. Часть дублей — host/proc-macro или разные dependency generations, а не два runtime в одном hot path. Удалить WebAssembly/Intl ради меньшего бинарника означало бы изменить продукт. Без доказательства лишней feature или runtime работы здесь не сделана такая подмена. Цена monomorphization/LTO отдельно не измерена; unit timings не доказывают её причину.

Выбранный feedback срез не добавляет generic framework или зависимости. Один classifier заменяет дублированные списки opcode в size planning и construction. Это сокращение путей поддержки; измеренного ускорения Rust-сборки ему не приписывается.

## Сопоставление трёх направлений

| Направление | Удаляемая стоимость и доказательство | Масштаб / цена / риск |
|---|---|---|
| Object/field SSA и virtual heap | В Box2D 13,6% Graph samples — tagged→FP checks, 5,4% — обратная упаковка. Потенциально исчезают conversions, allocations и recovery-дубли | Большой execution потенциал, но оба проверенных numeric прототипа отвергнуты. Полный virtual heap требует alias/effects/OSR materialization; высокий риск |
| Единые disjoint lifetimes и call constraints | Dense live-after имеет O(nodes×values) память; все calls spill даже C-preserved регистры | Потенциально меньше compile peak и spill traffic. Полный allocator/GC/deopt срез; средняя/высокая цена. Вклад в текущий wall ещё не выделен |
| Executable/source/feedback ownership | 24-байтовый compiler instruction и 32-байтовый VM instruction удерживаются вместе; 48-байтовый compile overlay; feedback aux enum 24 байта на каждый opcode плюс Box на каждый IC/call | Измеренные 80,15 МиБ SourceModuleBytes Mandreel. Первый законченный срез — плотные feedback arenas: удаление 16 байт/instruction и per-site allocations. Полное wordcode объединение шире и требует компонентного census |

Выбран первый срез направления 3: у feedback уже есть один CodeBlock owner и известная адресная граница с native code. Можно полностью заменить избыточное хранение, не вводя промежуточные owners, режимы или новый GC протокол. Numeric прототипы не продолжаются только потому, что уже написаны.

## Три архитектурных направления

### 1. Сквозная модель значений и объектов в оптимизаторе

Проблема: field summary знает Int32, nullish и shapes, но остальные primitives становятся Top. Числовые поля снова входят в SSA как Tagged; динамические проверки и упаковка повторяются вокруг операций. Отдельный escape pass допускает только `NewReceiver` и отвергает phi/lazy/loop-header случаи. Это теряет и типовые доказательства, и возможность убрать сам объект.

Новая граница: единая модель object/field facts и virtual heap в Graph, с точными effects и reconstruction. Поля и SSA должны сохранять доказанное числовое представление до точки, где JavaScript действительно требует Value. Неescaping allocations заменяются полями; escaping сохраняют обычную идентичность и GC boundary. В перспективе физическое typed storage может сократить размер Int32/Boolean полей; его нельзя незаметно подменить только новой битовой категорией.

Владельцы: runtime shape/field evidence отвечает за наблюдаемые факты и инвалидизацию; Graph за compile-local field/virtual-object state; code generation за recovery recipes. Никакой второй runtime registry. Изменение числа/boolean/object/descriptor либо проходит доказанный store, либо один committed runtime operation; предыдущие эффекты не проигрываются.

Первый сквозной implementation target: numeric field → SSA → arithmetic → field store на обеих ISA, затем единый virtual heap для literal/constructor allocations и OSR/deopt materialization. Отдельные микрооптимизации probe не являются итогом этого направления. Проверки: type widening, -0/NaN, prototype/accessor вмешательство, aliasing, inlined deopt и moving GC затронутого пути. Основные workload: box2d и raytrace; deltablue проверяет другое сочетание object/array операций, TS — контроль, а не единственный ориентир.

### 2. Один lifetime model вместо матрицы live-after и безусловных call spills

`graph/liveness.rs` хранит `placed_nodes × ceil(tracked_values/64)` words. При 100 тыс. узлов и 100 тыс. значений одна эта таблица требует примерно 1,25 ГБ. Это формула представления, а не утверждение о размере измеренного графа Mandreel. Кроме неё существуют bytecode register sets и flattened frame-state values.

Новая модель: CFG-aware disjoint live ranges с use positions и явно разнесёнными early/late точками. Их же используют register assignment, home interference/coloring, safepoint projection и deopt. Убираются per-node dense live-after rows и альтернативные hull-эвристики. Call constraints различают private JS clobbers, platform C clobbers и preserving leaves; collecting tagged values всё равно проходят canonical homes, которые GC может переписать. Callee-saved register не отменяет обязанность reload после moving GC.

Ожидаемый механизм выигрыша: меньше compiler peak memory, раньше готовый native code, меньше spill/reload и меньше frame/home footprint. Конкретный процент пока не измерен. Проверки должны включать branches/loops, OSR, eager/lazy recovery и реально collecting calls. Это полный allocator/lifetime срез, а не замена одного Vec.

### 3. Один владелец исполняемого тела и холодных source facts

На checkpoint `FunctionCode::Instruction` занимает 24 байта, а VM строит отдельный 32-байтовый `CodeBlockInstruction` с копией operands и дополнительными PC/function/site полями. При исполнении исходный linked bytecode продолжает удерживаться. Compile overlay добавляет ещё 48 байт на инструкцию во время снимка. Сюда не включены spans, CFG, typed feedback payloads и nested inline trees.

Новая граница: один immutable executable body, schema-driven operands и исходные function/site identities; runtime mutable feedback отдельно. Cold byte-PC/source layout и lazy/preparse facts не должны размножаться вместе с executable view или рекурсивными body snapshots. Нужно менять compiler producer, verifier, binary/image reader, interpreter и оба JIT consumer в одном срезе. Никаких compatibility readers или двойных writers.

Предполагаемая конечная граница этого следующего среза: `FunctionCode` владеет одним immutable backing из opcode/header + четырёх operand words, overflow-offset занимает первый word только для длинной формы. Канонический PC принадлежит frame/index, function identity — CodeBlock, dense IC numbering — его feedback directory; они не дублируются в каждом operand record. Compiler, wire tooling, verifier, interpreter и JIT читают одну форму. Нативные readers не получают wrapper allocation или generic-call detour. Физический lease должен находиться у того же shared body owner, что и массивы, чтобы escaped block/snapshot не удерживал весь source module и не терял charge после его drop. Clone делит неизменяемый body; mutable compiler/linker builder создаёт новый только при преобразовании. Такой переход требует нового одного текущего binary/serde формата и обновления всех consumers, без compatibility reader.

До выбора точного формата требуется разложить 80 МиБ SourceModuleBytes Mandreel на raw wordcode, built executable, feedback, source/atom/scope/preparse ownership. Найдено и ограничение текущего retained-byte описания: `Function::retained_bytes` не обходит `preparse`, хотя lazy records могут его держать. Поэтому ledger не считается полной физической census всей compiler metadata. Дополнительная проверенная граница покрытия: исходный LinkedBytecode sizing не включает динамические allocations `VerifiedBytecodeModule::functions` и per-function proof layouts. Новый wordcode owner учитывается отдельно и точно по своей геометрии; этим не исправлена вся историческая полнота compiler ledger. Это часть обязательного устранения дублирования и корректного владения, а не основание добавить ещё один приблизительный counter.

## Проверка выбранной модели и пересмотр границы

Первый вариант направления 1 сохранял Float64 даже у целочисленных результатов на всех tagged-границах, включая обычные массивы и Template. Он прошёл адресную семантику и native-пробы, но **отвергнут по полной матрице**. Пять AB/BA-пар на неизменённых fixtures дали:

| Класс | Wall before → after, с | Изменение | Peak RSS |
|---|---:|---:|---:|
| crypto | 0,2068 → 0,8707 | +321,0% | +0,2% |
| deltablue | 0,6654 → 0,6674 | +0,3% | +1,4% |
| box2d | 0,6182 → 0,8062 | +30,4% | +4,7% |
| raytrace | 0,7990 → 0,8152 | +2,0% | −0,4% |
| splay | 0,6646 → 0,6601 | −0,7% | −0,4% |
| regexp | 1,3336 → 1,3436 | +0,8% | −0,1% |
| mandreel | 1,0036 → 0,9925 | −1,1% | −1,3% |
| ts | 3,1609 → 3,1159 | −1,4% | −5,6% |

Это регрессия модели, а не повод подбирать JIT thresholds. В Crypto выросло и число retired instructions: медиана 3,85 → 14,14 млрд. Общая смена boxing загрязнила Integer-пути Float64-значениями; выигрыш удаления проверки упаковки не окупил последующие conversions и misses. Существующее unboxed Double storage обычных массивов не означает, что каждый получаемый из него Value должен быть Double. Бинарник, полный patch и сырые восемь рядов сохранены в `benchmarks/results/numeric-field-2026-10-22/`.

Второй вариант ограничил Float64 протокол прямыми Graph field loads/stores, сохранив canonical Number boxing на остальных границах. Оба backend прошли native проверки, но полные результаты снова не выполнили критерий:

| Класс | Wall before → after, с | Изменение | Peak RSS |
|---|---:|---:|---:|
| crypto | 0,2070 → 0,2304 | +11,3% | +2,4% |
| deltablue | 0,7153 → 0,6571 | −8,1% | −9,6% |
| box2d | 0,6353 → 0,6282 | −1,1% | +2,7% |
| raytrace | 0,8694 → 0,8323 | −4,3% | −0,5% |
| splay | 0,6623 → 0,6540 | −1,3% | −0,6% |
| regexp | 1,3639 → 1,3343 | −2,2% | −0,1% |
| mandreel | 1,0200 → 0,9975 | −2,2% | +0,5% |
| ts | 3,2640 → 3,2358 | −0,9% | −0,2% |

У основной цели Box2D выигрыш мал, память выросла. В отдельной require+двойной GC retention проверке TS медиана live GC bytes выросла 213,28 → 254,09 МиБ, RSS 513,52 → 533,05 МиБ; причинность этой вариации не установлена. Этот прототип также **отвергнут и полностью удалён**. Его exact patch, executable, диагностические artifacts и пять пар лежат в `benchmarks/results/numeric-field-direct-store-2026-10-22/`. На этих данных нельзя утверждать, что FP fields сами по себе плохая модель; можно утверждать, что оба конкретных среза цели не достигли.

Независимо выявлена существовавшая в checkpoint ошибка escape analysis: nested field merges читали ещё не созданный phi NodeId и обрушали compiler worker. В рабочем коде оставлено только её исправление: Graph владеет реальными phi с момента создания, они остаются unplaced до успешного анализа и удаляются при retry. `Rewrite` больше не хранит вторую копию inputs/representation. Regression `escape_nested_field_merges.js` воспроизводит падение checkpoint и совпадает с его interpreter после исправления. Это исправление корректности, ему не приписывается ускорение матрицы.

## Проверенный и отвергнутый срез: feedback arenas

Единственный owner — `CodeBlock::FeedbackVector`. На каждую инструкцию остаётся 4-байтовый recording cell и 8-байтовый family index вместо 24-байтового enum. Property, method, call и allocation payloads лежат в отдельных плотных boxed slices. Методная строка владеет и IC, и call feedback. Удалены per-site `Box` и обход пустых instruction slots при сканировании property IC. Размер backing storage вычисляется за O(1).

Плотная часть feedback уменьшена с **28 до 12 байт/instruction (−57,1%)**; это не процент RSS всего процесса. Существующие IC/call payloads остаются прежнего размера, header vector растёт с 40 до 104 байт на CodeBlock. Payload allocations теперь ограничены четырьмя family buffers на тело, независимо от числа sites. Все buffer allocations проходят существующий physical ResourceLease до publication; ошибки allocation не публикуют частичный block. Один opcode classifier определяет и geometry, и construction. Попутно устранён прежний пропуск payload `HasNamedProperty` в admission size planning.

Все slices замораживаются до выдачи адресов. Generated IC/call pointers на ARM64 и x86_64 по-прежнему указывают на прежние `PropertyIcSlot` / `AtomicCallFeedback` с неизменными layout и semantics. Lifetime равен CodeBlock; installed generations удерживают прежнего владельца. Native hit, guard miss и committed operation не менялись. Новых roots, safepoints, barriers, realm transitions или deopt recipes нет; weak IC maintenance проходит те же payloads плотным обходом. Ни старого enum, ни compatibility path в движке не оставлено.

Применимая модель из референсов: V8 отделяет FeedbackMetadata и slot families; JSC UnlinkedMetadataTable считает entries по opcode и фиксирует offsets до link. В Otter нужны их принципы классификации и стабильного адреса после publication. Собственный raw allocator, V8 heap types или второй metadata registry для этого не требуются.

Пять пар whole-process A/B: crypto −0,79%, deltablue −0,53%, box2d −0,29%, raytrace −2,68%, splay −0,36%, regexp −2,55%, mandreel −0,46%, ts −1,79% wall. Peak RSS Mandreel 223,14 → 227,94 МиБ, TS 570,52 → 601,92 МиБ. Диагностический embedded census показал SourceModuleBytes Mandreel 80,15 → 72,07 МиБ, но его старый executable содержит observer-прототип и не является performance baseline. Это полезное изменение геометрии, **цель полного процесса не выполнена**. Feedback arenas полностью убраны из активного кода; exact source patch и все исходные ряды сохранены в `benchmarks/results/feedback-arenas-2026-10-22/`.

## Проверенный и отвергнутый срез: диапазоны CFG liveness

Следующая подтверждённая избыточность — live-after матрица на каждый SSA node. Вместо копирования полного множества всех tracked values после каждого node один `Liveness` владеет отсортированными непересекающимися диапазонами на значение. Backward block dataflow остаётся источником истины для CFG, phi inputs читаются на incoming edges, eager/lazy states — на своём node. Обратный проход создаёт/закрывает диапазоны прямо на read/definition transitions; per-node bit matrix вообще не выделяется.

Позиция означает точку после node: final use заканчивает диапазон перед этой точкой, definition включает её, если значение читается дальше. Constants, phi, OSR и frame-state-only values сохраняют прежнюю точную CFG семантику. Все существующие safepoint, root-home и register-save consumers используют эти диапазоны. Новый формат не добавляет roots и не меняет native frame/ABI; обе ISA используют один анализ. Временные block sets по-прежнему имеют O(blocks×values) стоимость, поэтому это не заявление о линейности всей компиляции. Register-placement hulls и call clobbers пока не изменены; полный allocator срез из направления 2 остаётся следующим отдельным шагом.

Пять полных AB/BA-пар: Mandreel 1,0081 → 1,0023 с (−0,57%), peak RSS +0,40%; Box2D 0,6266 → 0,6169 с (−1,55%), RSS −0,53%; TS 3,1398 → 3,1569 с (+0,54%), CPU 4,24 → 4,36 с, RSS −5,61%. TS post-GC снова дал вариацию 213,28 → 254,41 МиБ live cells. Во всех случаях результаты совпали, GC strides и обе ISA прошли. На фактической матрице цель не достигнута; реализация убрана. Сохранённый source patch и executable: `benchmarks/results/liveness-ranges-2026-10-22/`.

## Текущий сквозной срез: один shared wordcode owner

Compiler/module и CodeBlock теперь разделяют один `FunctionCode` с immutable массивами и физическим lease внутри того же shared owner. Нет второго массива 32-байтовых VM records и второй overflow operand table. Канонический record остаётся 24-байтовым; `CodeBlockInstruction` — только временный borrowed fetch view, который нигде не хранится по инструкции и не выделяется в heap. PC выводится из индекса, source function identity хранится один раз у executable view. Isolate-local IC numbering занимает четыре байта на инструкцию.

Геометрия сохранённых инструкций меняется с 24+32=56 до 24+4=28 байт/instruction, без учёта общих feedback/CFG/source tables. Сам shared owner имеет отдельный header/lease: 72 байта плюс минус 24 байта в `Function` за замену двух fat pointers одним Arc. Это не обещание 50% RSS: tiny cold bodies могут стоить больше, большие built bodies — меньше. Compile snapshots и fresh-isolate copies разделяют immutable code и charge, создавая новые feedback/source-work/IC metadata. Mutable builder отделяет слова до изменения, уже admitted clones сохраняют один owner.

Текущий binary/serde wordcode по-прежнему описывает те же канонические слова; work weight восстанавливается из opcode в header padding и не хранит VM identity. Никаких compatibility formats/readers или VM execution копии. Physical admission не меняет verified instruction bytes/PC facts. Отказ уничтожает unpublished owner и lease; внешний unadmitted compiler alias отделяется до admission, чтобы не удержать charge отказавшей публикации.

Для этого среза напрямую применима граница JSC: `CodeBlock::instructions()` возвращает stream из `UnlinkedCodeBlock`, а не второй массив инструкций. V8 `BytecodeArray` связывает исполняемые bytes с constant/handler/source metadata и проверяет доступность до публикации. В Otter выбран Rust shared owner с inseparable resource lease, отдельными isolate IC/feedback и canonical PC из index. GC object layout и sandbox wrappers V8 не переносятся; они решают другую границу памяти.

## Очерёдность следующих сквозных срезов

Текущий owner после рефактора:

```mermaid
flowchart LR
    B[Compiler/linker builder] -->|freeze| W[Immutable wordcode + physical lease]
    M[Verified module] --> W
    C[CodeBlock: isolate metadata] --> W
    C --> F[Feedback + IC directory + source-work]
    J[JIT snapshot] --> C
    S[Fresh isolate CodeBlock] --> W
    S --> N[Independent feedback and source-work]
```

### 1. Закрыть shared wordcode как основу владения

Это текущая реализация: `otter-bytecode/wordcode{,_storage}.rs`, `verifier.rs`, VM `execution_code.rs`, `executable.rs`, `executable_snapshot.rs`, `code_space{,_snapshot}.rs`. Producer/consumer boundary уже одна, старый VM instruction массив удалён. Canonical record не хранит function id и PC; borrowed view их не дублирует по всему телу. При одновременно живых module и executable instruction storage уменьшается с 56 до 28 байт/op; холодные never-built тела получают дополнительный shared-owner header. После удаления module у единственного escaped block сравнение другое: 32 против 28 байт/op плюс header. Размеры не выдаются за проценты всего RSS.

После финального gate этот срез не следует продолжать настройкой индексов ради долей процента. Оставшиеся дубли span/verified layout/scope metadata требуют компонентной census, а не механического переноса всего module под один Arc: такой Arc удерживал бы чужие функции ради одной escaped body.

### 2. Закрыть причину retained JS graph

Следующий resource gate должен различить активный require/GC frame и quiescent runtime boundary. Нужен адресный root-owner census: globals/contexts, активные frame homes, pending compile roots и retained generations. Состояние снимается вне timed workload, с bounded owned output, без per-call counters. Наблюдаемые шаги по 41 МиБ нельзя объявлять ни новой утечкой wordcode, ни безвредным allocator noise: меняется reported live JS heap. Пока путь до корня не установлен, общий retention результат TS остаётся открытым.

### 3. Native object/field model: один virtual heap и reconstruction

Это следующий приоритет исполнения: текущие профили показывают 56–72% isolate samples в Graph. В `graph/effects.rs` уже существует дерево abstract heaps, включая отдельные Field(atom), MapWords, Movement и dependency facts; его надо развить, а не объявлять отсутствующим и написать второй alias pass.

Первый вертикальный срез должен оставить действующий физический Value layout и заменить ограниченный constructor-only анализ единой compile-local моделью allocation identity + field/element SSA. `graph/escape.rs`, `types.rs`, `effects.rs`, `redundancy.rs` и frame-state reconstruction используют одни факты. По CFG join создаются реальные Graph-owned phi. Literal objects/arrays и NewReceiver проходят один протокол; при неизвестном alias, JS reentry, prototype/accessor наблюдении или неприменимом OSR анализ сохраняет физическую allocation до эффекта.

Eager/lazy reconstruction обязана хранить virtual identity и актуальные fields на конкретном source PC. Materialization создаёт все identity cells до заполнения ссылок, чтобы повторные ссылки и циклы давали один объект, и заполняет через действующие rooted context APIs. Удалённый store не может оставлять ложный runtime field-type proof: compile-local facts для ещё не опубликованного receiver принадлежат virtual heap, не истории опубликованных shapes. Стоимость, которая должна исчезнуть, — сами allocations, field loads/stores и их barriers/boxing. Это проверяется по allocation traffic, native IR и полному времени Box2D/DeltaBlue/RayTrace; TS остаётся дополнительным compile/retention контролем.

Следующий самостоятельный срез — **физический field layout, которым владеет shape**. В нём shape guard доказывает ширину/вид slot; Int32/ref fields могут занимать четыре байта, Number — восемь без Value-tag decode, смешанные unions — обычный Tagged word как законный вид поля текущего формата. Менять вид поля в существующем shape нельзя: widening публикует новый layout/shape. Inline capacity следует задавать в байтах; widening перераскладывает банки/overflow, сохраняя identity объекта. Все allocating preparations и roots предшествуют участку перестановки; новый shape публикуется последним, без collecting boundary посреди изменения layout.

Это меняет VM object/shape/storage, property IC/native readers обеих ISA, GC slot visitors и deopt materializer вместе. GC читает reference layout из live shape, а не угадывает pointers по числовому word. Canonical Number boxing на внешних Value/array/call границах сохраняется: именно его загрязнение сломало первый эксперимент. Риски — layout churn/число shapes, переходы wide→generalized, -0/NaN, prototype-role mutations, cycles и moving roots. До реализации нельзя обещать процент выигрыша. Gate включает адресные descriptor/type-widening/alias tests, materialization с GC strides 1/4/16, обе ISA и одинаковые результаты interpreter.

### 4. Общий lifetime/call protocol после количественного подтверждения

`graph/regalloc.rs` и `registers.rs` сейчас опустошают регистры на call, включая C-preserved GP. При этом String charCodeAt и Math arithmetic уже имеют специализированные Graph nodes; нельзя приписывать им цену обычного C leaf. Сначала нужно выделить реально исполняемые call families и их spill traffic по native artifacts. Новая граница различает private JS, collecting runtime и passive C leaf, сохраняя canonical homes и recovery contract каждого домена.

Далее use positions, disjoint CFG ranges, register assignment, spill-home interference и safepoint projection должны читать один lifetime model. Отдельная замена live-after matrix уже проверена и отвергнута как самостоятельный performance срез. В текущих baseline captures Box2D/DeltaBlue крупнейшие оценённые node matrices имеют порядок 80/56 КиБ; формула 1,25 ГБ для абстрактного 100k×100k графа не доказывает их bottleneck. Полный allocator рефактор выбирается после реального compile-peak/call-clobber census, а не по этой формуле.

После каждого цельного среза: один regression case, узкие существующие фильтры, interpreter oracle, затронутые GC strides 1/4/16 при изменении roots, отдельные одинаковые production CLI builds и 3–5 чередующихся A/B. Матрица покрывает compute, object/array, string/collection, compile и retention. Gate — одинаковая работа и результаты, wall + CPU + peak RSS + post-GC, без существенной memory regression. Проценты до реализации не обещаются. Полные workspace tests, all-features Clippy и полный Test262 автоматически не запускаются.

## Референсы из исходников

Локально прочитаны и зафиксированы ревизии, а не только статьи:

* V8 `926c0e5fb9cd60257e49cbdf7855ba42dd13963c`: [UseInterval/LiveRange и splitting](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/backend/register-allocator.h), [SharedFunctionInfo/PreparseData](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/shared-function-info.h), [FeedbackVectorSpec/FeedbackMetadata](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/feedback-vector.h), [BytecodeArray](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/bytecode-array.h). Применимы disjoint ranges и самостоятельные source/function owners; механический перенос GC object types не нужен.
* JavaScriptCore `e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e`: [ObjectAllocationSinkingPhase](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGObjectAllocationSinkingPhase.cpp), [UnlinkedMetadataTable](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/UnlinkedMetadataTable.h), [UnlinkedCodeBlock](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/UnlinkedCodeBlock.h), [CodeBlock::instructions](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/CodeBlock.h#L339). LocalHeap, allocation SSA и OSR availability связывают escape analysis с восстановлением; это полезнее набора constructor-specific исключений. Цена для Otter — полноценная field-state модель на CFG и materialization в deopt.
* SpiderMonkey `5836a062726f715fda621338a17b51aff30d0a8c`: [Nursery](https://github.com/mozilla/gecko-dev/blob/5836a062726f715fda621338a17b51aff30d0a8c/js/src/gc/Nursery.cpp). Nursery placement, promotion и store buffer — согласованный протокол. В Otter немедленное transitive promotion из old parents компенсирует object-granular rescans; убрать его отдельно без range/card ownership было бы опасной «оптимизацией константы».

## Где доказательства

`benchmarks/results/architecture-census-2026-10-22/`: manifest, source patch, хеши executable и fixtures, stdout/stderr всех семи census запусков; отдельные `profile-{deltablue,box2d,mandreel}.txt`; offset profiles и exact native artifacts двух object workloads. Census — расширение существующего allocation probe, только на явном диагностическом запуске. Обычное исполнение не получает новых counters, maps или проверок.

Сборочный анализ и прежние эксперименты сохранены в `benchmarks/results/metadata-ownership-2026-10-21/`. Там одинаковые release-параметры, baseline executable, Cargo timings/features/dependency graph, checkpoint diff и неудавшиеся гипотезы. Старый сентябрьский архитектурный отчёт полезен как история, но его object/context sizes и старые проценты не выдаются за текущее состояние.

Финальный CLI соответствует `final/compiled-source.patch`; `final/post-build-format-docs.patch` фиксирует последующее форматирование одной Rust expression и уточнение docstring без изменения исполняемых выражений/типов. `final/source.patch` содержит полный итоговый working-tree diff. Raw manifests сохраняют отдельные SHA бинарников и patch.
