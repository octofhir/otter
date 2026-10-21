# Завершение RegExp-поиска по полному литералу

Локальное исследование Otter, 21 октября 2026, Apple M1 / macOS ARM64.

Устранён повторный запуск matcher для доказанного полного литерала. Итоговые
steady kernels ускорились на **5.1%** (mixed) и
**6.2%** (UTF-16). На полном Octane RegExp эффект
скромный: парная медиана **-0.54%** по 17 before/after
парам. Instructions на RegExp: **-1.55%**. Стандартные
контроли не дают общего выигрыша. Разрыв с Node на `ts`: 2.74× → 2.74×.


## Причина затрат и выбор участка

`ts` — `benchmarks/fixtures/fixed-work/ts.js`: восемь запусков старого
TypeScript-компилятора из Octane над встроенным `compiler_input.ts`, с emit ES5,
проверкой числа ошибок и checksum вывода. Это JavaScript-нагрузка на движок.
Исходное рабочее дерево уже содержит параллельные изменения GC, объектов,
словарей, Graph и CLI. Они включены в baseline, а сравнение использует отдельный
снимок исходников и замороженные исполняемые файлы.

Профиль `regexp` содержит 831 выборку isolate-потока. Два `advance_native`
вместе занимают 198 inclusive samples (23,8%); это все программы matcher,
а не только выбранное подмножество. `find_units` имеет 21 self sample (2,5%).
Неизвестные generated PCs не приписываются целиком RegExp.

| Кандидат | Подтверждение и частота | Верхняя граница / ограничения | Ресурсы, сложность, уверенность |
| --- | --- | --- | --- |
| Повторная проверка полного литерала в matcher | Native search 23,8% inclusive; отдельный protocol-census: 52 подходящих шаблона, 2 705 295 успешных exec | Менее совокупных ~26% поиска и подготовки: убирается лишь часть программ | Исключает native mappings и scratch-векторы на этом пути; небольшой независимый срез; средняя уверенность до A/B |
| Создание match results | `try_substrings_no_collect` 34 self samples + `build_match_result_folded` 19: 6,4% self | Эти 6,4% нельзя целиком удалить: строки и массивы наблюдаемы | Больше GC/escape-analysis рисков; пересечение с работой над объектами; средняя уверенность |
| Вход в replacement protocol | `regexp_symbol_replace` closure 40 self samples: 4,8% | Полный inclusive стек содержит matcher и результаты; его нельзя сложить с первой строкой | Требует доказать причины отказа pristine path и порядок getters/callbacks; средняя/низкая уверенность |
| Создание литералов RegExp / compile cache | `jit_load_regexp_stub` 32 inclusive samples: 3,9% | Идентичность новых RegExp наблюдаема; частота реальных аллокаций отдельно не измерена | Нужна escape proof; низкая уверенность в устранимой доле |

Census выполнен отдельным нетаймированным Node-прогоном с обёрткой `exec`;
это число protocol-вызовов, а не счётчик Rust-входов Otter. Особенно это различие
важно для split: Otter уже объединяет его sticky-пробы в поиск. Классификация
делается собственным parser/lowering RegExp через `--dump`, без разбора JS regex-ами.
Проценты в рейтинге являются границами потенциальной работы, а не обещанием ускорения.

## Реализованный механизм

Исходный JS (`exec`, `test`, строковые regex-методы) проходит прежние bytecode,
feedback, Graph/Template guards и VM-протоколы. Изменяется один нижний владелец:
`otter-regex`, вызываемый из `regexp_fast::exec_direct` и `find_batch`.

Lowering доказывает одновременно: весь instruction stream — case-sensitive
`Char` либо `CharSeq` и `Match`; declared capture count равен нулю; Unicode mode
выключен; needle непустой и не усечён. Для такого compiled pattern существующий
required-literal scanner уже доказывает оба конца совпадения. Повторный вход в
backtracker исчезает вместе с подготовкой native State, stack/log/capture vectors
и компиляцией native regex code.

У поиска один `Execution` plan: literal, native либо interpreted. Compiled pattern
владеет scanner: одиночный unit хранится inline и использует `Unit::position`,
последовательность владеет подготовленными finders через один Box. Эта подготовка
не выполняется на каждый поиск. Два result words остаются в bounded isolate scratch.
Для последовательности добавлен один Box на подготовленный scanner; полного
allocation census этого изменения нет.

Sticky-поиск проверяет только свою позицию и сохраняет прежний счёт попыток,
включая несовпадение хвоста после подходящего первого unit. Исчерпание бюджета
возвращает прежнюю ошибку. Остальные программы используют matcher. Явные test-tier
поиски обходят literal completion, сохраняя независимый backtracker для проверки.

JS-visible API, lastIndex/coercion/getter ordering, RegExp statics, realms,
result builders, handle scopes, write barriers, safepoints, deopt, OSR и ABI
не меняются. Новых JS API или TypeScript declarations нет. Общий Rust scanner
исполняется на обеих JIT-архитектурах; C ABI emitters не изменялись.

Два найденных при проверке риска закреплены регрессиями:

* Байтовый finder для одиночного UTF-16 unit повторно находил нечётные byte hits
  на `"Xā".repeat(256) + "Ā"`. Промежуточный вариант увеличивал retired instructions
  примерно в 5,3 раза. Итоговый unit scanner сравнивает целые code units.
* `(a){0}b` не содержит `Save`, но сохраняет declared capture. Допуск по одним
  инструкциям терял `undefined`-слот; для named `/d` это также угрожало indices
  builder. Итоговый допуск требует `group_count == 0` и сохраняет общий путь.

Код: [допуск полного литерала](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-regex/src/program.rs:434),
[завершение поиска](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-regex/src/api/literal.rs:21),
[matcher-регрессии](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-regex/src/api/literal_tests.rs),
[JS protocols](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-difftest/corpus/regexp_literal_scan.js),
[описание механизма](/Users/alexanderstreltsov/work/octofhir/otter/docs/site/src/content/docs/engine/regexp-search.md).

## Измерения

Пять независимых measured-процессов на engine/case, перед ними один нетаймируемый процесс. Порядок before/after/Node вращается в каждом блоке. Все checksums и stdout совпали. Стресс, trace и event capture выключены. Это свежие VM при прогретом OS file cache. В tables: медиана [min–max].

| Нагрузка | Before, ms | After, ms | Δ медиан | Node, ms |
| --- | ---: | ---: | ---: | ---: |
| Пустой запуск | 11.51 [11.28–12.34] | 11.06 [10.80–12.90] | -3.92% | 28.89 [28.44–29.78] |
| Literal mixed, процесс | 233.24 [231.15–239.49] | 221.63 [220.82–222.10] | -4.97% | 133.47 [133.21–137.44] |
| Literal UTF-16, процесс | 212.28 [211.22–212.80] | 200.97 [200.04–204.94] | -5.33% | 3978.99 [3955.19–3995.32] |
| Octane RegExp | 1286.33 [1283.90–1342.67] | 1278.77 [1267.61–1284.74] | -0.59% | 408.56 [407.12–416.38] |
| Octane TypeScript | 3196.87 [3143.03–3271.56] | 3195.38 [3163.52–3213.56] | -0.05% | 1167.26 [1149.15–1309.73] |
| Crypto | 200.17 [198.99–206.08] | 200.37 [199.73–200.78] | +0.10% | 158.95 [156.66–159.30] |
| Zlib | 1749.60 [1745.33–1751.63] | 1753.13 [1747.74–1759.95] | +0.20% | 1614.64 [1610.11–1703.67] |
| Earley–Boyer | 1464.38 [1456.07–1474.28] | 1462.48 [1456.52–1469.27] | -0.13% | 1037.44 [1036.79–1049.40] |
| Richards | 1041.56 [969.00–1047.67] | 1036.84 [974.36–1042.59] | -0.45% | 634.77 [633.48–640.98] |

Первые вызовы kernel включают его tier-up; затем записаны шесть warmup-вызовов и десять steady-вызовов. Единица агрегации ниже — медиана внутри каждого из пяти процессов, а не 50 независимых наблюдений. Source parsing и подготовка inputs находятся вне kernel clock, но включены в process table.

| Kernel | First before → after, ms | Steady before, ms | Steady after, ms | Δ steady |
| --- | ---: | ---: | ---: | ---: |
| literal | 14.41 [14.08–14.81] → 13.63 [13.43–13.69] | 13.58 [13.48–13.85] | 12.88 [12.81–12.91] | -5.11% |
| literal-wide | 12.88 [12.77–13.01] → 12.19 [12.02–12.37] | 12.35 [12.19–12.37] | 11.59 [11.44–11.87] | -6.16% |

Для небольшого эффекта полного RegExp добавлен второй, заранее ограниченный набор из 12 process-блоков: before 1281.15 [1272.43–1381.35], after 1272.01 [1262.48–1292.34] ms. В двух наборах парная медиана изменения — **-0.54%**, 13/17 пар быстрее. Описательный непараметрический интервал медианы пар: [-1.61%; -0.17%], покрытие 95.1% по порядковым статистикам. Это небольшая локальная оценка, не крупный выигрыш всего workload. Оба набора и выбросы сохранены.

UTF-16 case специально провоцирует half-unit byte hits. Большой разрыв с Node в нём уже существует у baseline и не является результатом этого изменения. Контрольные workloads не показывают устойчивого общего ускорения.

| Нагрузка | Retired instructions Δ | CPU user before → after, s | Peak RSS before, MiB | Peak RSS after, MiB |
| --- | ---: | ---: | ---: | ---: |
| Пустой запуск | +0.07% | 0.00 [0.00–0.00] → 0.00 [0.00–0.00] | 21.36 [21.31–21.41] | 21.42 [21.42–21.48] |
| Literal mixed, процесс | -9.42% | 0.22 [0.22–0.23] → 0.21 [0.21–0.21] | 28.33 [28.31–28.34] | 28.33 [28.31–28.34] |
| Literal UTF-16, процесс | -10.02% | 0.20 [0.20–0.20] → 0.19 [0.19–0.19] | 28.27 [28.22–28.31] | 28.34 [28.31–28.38] |
| Octane RegExp | -1.55% | 1.28 [1.28–1.31] → 1.27 [1.26–1.28] | 143.11 [142.80–143.83] | 142.70 [142.12–142.84] |
| Octane TypeScript | +0.14% | 4.10 [3.98–4.15] → 4.09 [4.03–4.12] | 567.94 [486.50–600.22] | 549.86 [485.42–607.45] |
| Crypto | -0.14% | 0.20 [0.20–0.21] → 0.20 [0.20–0.21] | 44.02 [43.03–46.06] | 45.67 [43.70–46.06] |
| Zlib | +0.01% | 1.72 [1.72–1.73] → 1.73 [1.72–1.73] | 116.31 [116.27–116.33] | 116.42 [116.41–116.47] |
| Earley–Boyer | +0.29% | 1.49 [1.49–1.51] → 1.49 [1.49–1.51] | 122.39 [122.02–124.06] | 122.34 [121.52–123.44] |
| Richards | -0.00% | 1.03 [0.96–1.04] → 1.03 [0.97–1.03] | 41.89 [41.28–42.41] | 41.91 [41.77–42.33] |

CPU counters для очень короткого startup квантованы. Instructions не используются вместо измеренного времени. Peak RSS измерен `time -l`, это процесс целиком, а не GC heap.

Однократный рост median crypto RSS не воспроизвёлся во втором наборе: before 44.71 [43.30–45.75], after 44.04 [42.86–46.30] MiB. Диапазоны перекрываются; причинный рост или общее снижение памяти не заявляются. TS RSS также имеет широкий разброс.

Отдельные диагностические RegExp-прогоны, n=3 на вариант, не входят в timed samples:

| Ресурс | Before | After |
| --- | ---: | ---: |
| JS compile events | 28.000 [28.000–28.000] | 28.000 [28.000–30.000] |
| Сумма JS generated code, bytes | 2644988.000 [2644988.000–2644988.000] | 2644988.000 [2644988.000–2716700.000] |
| JS compiler-hook time, ms | 32.819 [32.584–33.737] | 34.115 [33.307–35.060] |
| Minor GC count | 98.000 [98.000–98.000] | 98.000 [98.000–98.000] |
| Minor service total, ms | 7.802 [7.690–8.308] | 8.150 [7.468–8.555] |
| Old occupancy maximum, MB из trace | 76.800 [76.800–76.800] | 76.800 [76.800–76.800] |

Capture на RegExp не усечён. After в одном из трёх запусков компилирует два дополнительных JS поколения; это +71 712 generated bytes в том запуске. Hook-time диапазоны перекрываются; экономия JS compilation не заявляется. Native regex mappings в эти JS code bytes не входят. Literal-планы вообще не вызывают native regex compiler, однако exact bytes освобождённых mappings отдельно не измерены. GC count/occupancy совпадают; service-time диапазоны перекрываются.

## Проверки и пределы вывода

* `cargo test --release -p otter-regex`: 47 unit + 49 integration tests, **96/96**; то же **96/96** на `x86_64-apple-darwin` под Rosetta.
* Финальный changed-crate Clippy: все targets/features, `-D warnings`, passed.
* `cargo test --release -p otter-vm --lib`: **1283/1283**.
* `otter-difftest` на каноническом executable: **188/188**, interpreter oracle, Template, production, GC stress 1/4/16 и slot verification.
* Дополнительные три RegExp-corpus cases: **144/144** в interpreter/Template/production, strides **1–16**, `OTTER_GC_VERIFY=1`; итоговый `status: passed`.
* Targeted Test262: **2290** одинаковых path/outcome rows before/final, **2271 pass, 19 skip**, zero failures/crashes/timeouts/OOM. Это RegExp включая Annex B и String replace/replaceAll, split, match/matchAll, search.
* Новый JS corpus проверяет lastIndex, `/d`, named absent captures, zero-quantified declarations, prototype override, readonly/coercing lastIndex, compile replacement и string protocols.

Полный repository gate имеет исходное падение
`graph::builder::field_tests::field_type_tests::collecting_effects_repeat_owner_and_target_guards_without_rechecking_targets`:
получен один `CheckFieldTypes`, тест ожидает два. То же падение воспроизведено
с исходным regex-кодом из baseline. Эта проверка строит Graph из искусственного
bytecode с `NewObject`; чужие изменения не исправлялись. Поэтому общий gate
не объявляется прошедшим. Его fmt и workspace Clippy прошли; дальнейшие стадии
после отказа JIT unit test не выполнялись самим gate.

Gate применяет `cargo fmt --all` к снимку. Его изменения девяти чужих файлов
сохранены в `gate-formatter.diff` и возвращены перед benchmark build. В итоговом
performance-снимке отличаются только предназначенные для этого среза engine-файлы.
Verification-снимок с форматированием отдельно идентифицирован манифестом;
основное рабочее дерево чужих изменений не получало.

Данные ограничены M1. x86-64 проверен функционально под Rosetta; throughput на
native x86-64/Windows не измерялся. Нет отдельного полного allocation census,
измерения exact retained native-regex mapping bytes или финальной retained-heap
фотографии после forced full GC. Неуспешный layout-probe не используется как
измерение. GC occupancy и peak RSS не подменяют retained-memory доказательство.
Capture JIT events на `ts` достигает лимита 16 384, поэтому число 800 компиляций
477 функций по нему не подтверждается и не опровергается.

Следующий наиболее перспективный независимый шаг — подготовка native matcher
State один раз на поиск и уменьшение повторной подготовки между результатами
batch. До изменения нужен свежий профиль оставшихся non-literal patterns;
23,8% исходного inclusive matcher времени не является обещанным выигрышем.

## Воспроизведение и артефакты

HEAD: `0c56affaa54f481a3dcd3b50a29fc309f4e8b305` **плюс исходный рабочий diff**, а не чистый commit.
Apple M1, `rustc 1.100.0-beta.7 (fd8e211ae 2026-10-17)`, Node `v24.16.0`.
Release: opt-level=3, thin LTO, codegen-units=1, debug=1, assertions off; каждому
executable установлен его embedded startup image. Performance использует обычную
production tier policy. `NODE_OPTIONS`, `RUSTFLAGS`, `CARGO_BUILD_TARGET`,
`RUSTC_WRAPPER` не установлены; harness отвергает любые OTTER overrides.
Процессы compilation/test/profiling во время timing не запускались. Desktop
background load сохранён `ps` перед каждым process-блоком; измерение не является
изолированным bare-metal стендом.

* [Environment и hashes](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/environment.json)
* [Исходный diff](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/baseline-working.diff), [исходный status](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/baseline-status.txt)
* [Baseline source manifest](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/source-manifest.json), [final source manifest](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/candidate-source-manifest.json)
* [Baseline source archive](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/baseline-source.tar.gz), [final source archive](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/candidate-source.tar.gz)
* [Before executable](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/before-otter), SHA-256 `ed115dca3355a2f65df465ae997c4725860bb6a9046af9e6875e7812ff9dfceb`
* [Final executable](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/after-verified-otter), SHA-256 `c1421a4479281218d313e4726afee39e8aa0b36e82ba897d6bbca82b8f0414ca`
* [Timed raw samples / commands / counters](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-ab/results.json), [confirmation](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/confirm-ab/results.json)
* [Resource diagnostics](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-resources.json), [raw profile](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/regexp-sample.txt), [profile summary](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/regexp-profile-summary.json)
* [Protocol census](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/regexp-census.json), [final admitted subset](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-literal-census.json)
* [Differential report](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-difftest.json), [stress report](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-stress/results.json), [conformance path comparison](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/verified-conformance-comparison.json)
* [Own patch](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/own-change.patch), [gate failure](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/final-repository-gate.log), [same baseline failure](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/regexp-search-2026-10-21/baseline-gate-failure.log)

```sh
python3 scripts/dev/regexp-literal-ab.py \
  --before benchmarks/results/regexp-search-2026-10-21/before-otter \
  --after benchmarks/results/regexp-search-2026-10-21/after-verified-otter \
  --out benchmarks/results/regexp-search-2026-10-21/replay-ab --runs 5

python3 scripts/dev/regexp-literal-ab.py \
  --before benchmarks/results/regexp-search-2026-10-21/before-otter \
  --after benchmarks/results/regexp-search-2026-10-21/after-verified-otter \
  --out benchmarks/results/regexp-search-2026-10-21/replay-confirm \
  --runs 12 --fixtures regexp crypto
```

Для пересборки развернуть соответствующий source archive в отдельную workspace
с сохранёнными `Cargo.lock` и toolchain; добавить vendor/Test262 для conformance.
Точные cargo-команды сохранены в build/test command manifests. При переключении
снимков обновлять mtimes либо использовать свежий target directory: одного
сравнения content hashes недостаточно для Cargo freshness. После build проверить
source manifest, скопировать binary и установить startup image в копию.


Первый `initial-ab` недействителен: сохранённые mtimes позволили Cargo использовать
старый артефакт. `final-ab` — промежуточный вариант с UTF-16 регрессией, также не
используется в итоговой оценке. Их raw данные и binaries сохранены. К окончательному коду относятся
`verified-ab` и `confirm-ab`; промежуточные `after-final-otter`/`after-unit-otter`
не включают окончательную metadata-проверку и также исключены.
