# Shape-owned data loads — 2026-10-21

## Результат

В действующем Graph заменён путь мегаморфного own-data чтения: `LoadShapedProperty` получает слот из полного индекса текущей формы. Generated code больше не требует присутствия такой пары shape/atom в shared action cache. Само чтение стало чистой операцией над map words и конкретным полем, поэтому Graph объединяет повторные чтения и сохраняет факты через чтение. Это изменение VM directory, IR, effects, GVN/LICM, typed recovery и обоих native backends; входной benchmark не менялся.

Полный `ts`: **3243.09 → 3047.60 мс (-6.03%)**. После быстрее в 5/5 пар; медиана парных изменений -6.14%. Node остаётся быстрее: 1221.87 мс. Прирост peak RSS — наблюдаемый расход этого набора, не доказанное удержание памяти после GC.

## Воспроизводимость

Commit `0c56affaa54f481a3dcd3b50a29fc309f4e8b305` плюс исходный dirty diff: `benchmarks/results/ts-slice-2026-10-21/starting.diff`. Каноническое исходное состояние — `inherited-source-manifest.json`, финальное — `shape-final-source-manifest.json`. Изменения сохранены в `shape.patch` и `shape-final-source.tar.gz`. Измеренные бинарники: `before-otter`, `after-otter`; SHA-256 и версия Node записаны в `final-ab/results.json`. CLI собран отдельно от Test262 командой `cargo build --release -p otter-cli --target-dir .../target`; JSON Cargo artifact подтверждает producer и executable. Перед измерениями в binary записан свежий startup image. Production tier policy, без OTTER-override и диагностического capture.

Платформа: Apple M1, macOS 26.5, 16 GiB; rustc 1.100.0-beta.7; Node 24.16.0. `ts` — полный fixed-work запуск старого TypeScript compiler из Octane: восемь исходных parse/typecheck/emit итераций и встроенная проверка результата. Это не TypeScript frontend Otter. Один прогрев процесса исключён, затем пять отдельных процессов на engine; порядок before/after/Node вращается. Все stdout, exit codes и проверки результата совпали. Сырые time(1), CPU/RSS/instruction данные, команды и снимки процессов сохранены в `final-ab/`.

Во время A/B Rust-сборки завершились. OrbStack Helper занимал примерно одно CPU-ядро, WindowServer оставался активен; снимки процессов сохранены. Before имеет выброс 3756 мс; он не исключался. Новый большой профиль, отдельные cold/steady серии и набор других workloads по последнему запросу пользователя не запускались. Пять процессов — ограниченная локальная оценка, не универсальная оценка всех приложений.

| Полный ts | Before | After | Node |
|---|---:|---:|---:|
| Wall, мс: медиана [min–max] | 3243.09 [3176.90–3755.92] | 3047.60 [3026.02–3159.46] | 1221.87 [1165.46–1247.29] |
| CPU user, с: медиана | 4.17 | 4.05 | 2.35 |
| Retired instructions, млрд: медиана | 35.197 | 34.287 | 20.890 |
| Peak RSS, МиБ: медиана [min–max] | 501.38 [484.95–614.56] | 582.05 [528.61–618.56] | 504.67 [504.16–505.14] |

Instructions: -2.58%; peak RSS по медианам: +16.09%. Диапазоны RSS широко перекрываются; максимум изменился с 614.56 до 618.56 МиБ. Дополнительный индекс стоит ровно 12 bytes на capacity entry существующей lineage table. Отдельные post-GC retention, allocation counters, итоговый объём native code и compile duration финального ts не измерены; улучшения по ним не заявляются.

## Причина и выбор среза

Базовый профиль: 6659 isolate samples, Graph 48.7%, runtime 24.4%, Template 8.5%, GC 4.8%, собственно JIT compiler 1.5%. Отдельный запуск с точными JIT artifacts дал 233 samples `LoadPropertyCached` из 1571 Graph samples; особенно повторялся `nodeType` в обходах AST. Artifact capture существенно увеличивает compiler overhead (14.2% вместо 1.5%), поэтому его wall time не сравнивается с production wall и проценты не смешиваются как точные доли одной нагрузки.

| Кандидат | Подтверждение и предел прямой части | Решение |
|---|---|---|
| Полный shape-owned own-data lookup и чистые reads | 233/1571 Graph samples в artifact-run; прямое устранение ограничено этой частью, вторичный эффект на spills и GVN заранее не определён | Реализован; память +12 bytes/capacity entry, без per-site таблиц |
| String receiver leaves / length | В обычном профиле String charCode leaf ~47 samples, substring boundary ~51; прямые доли малы. Отдельный эксперимент дал ~2.4% ts | Не включён в финальный патч |
| Dictionary/indexed access | LoadElement boundary 280/6659 samples, StoreElement 211/6659; это верхняя граница всей соответствующей runtime-части, не доказанная доля dictionary | Параллельный владелец; механизм не изменён |
| Constructor stores / allocation | 279/1571 Graph samples StorePropertyCached в artifact-run, плюс allocation/GC; доли перекрываются, их нельзя складывать | Параллельные S1–S5; store/receiver протоколы не изменены |

Перед выбранной моделью проверены и отклонены front-cache, retained PIC и pure action-cache-load варианты: первый находился в шуме, остальные замедляли ts. Pure cache loads выходили при вытеснении кэш-записи, поэтому индекс полной формы стал источником разрешения, а не очередной кэш перед прежним кэшем. Архивы отвергнутых source-вариантов и A/B остаются в results.

## Механизм и инварианты

Существующий shared lineage directory остаётся единственным владельцем списка ключей. Его u64 atom bucket несёт полный atom, logical slot + 1, accessor bit и flags. Rust descriptor lookup и оба native emitter читают один payload без зависимого чтения KeyEntry. Родитель ограничивает видимую часть собственным property count; append потомка ему невидим. Короткие формы сохраняют ограниченный обход transition chain. Новых runtime stacks, ABI carriers, versioning или adapters нет.

Нативная проба проверяет ordinary body, ShapeState, data descriptor, prefix bound и текущую inline/suffix capacity. В ней нет вызовов, allocation, safepoints или raw pointers, переживающих операцию. Только tagged result выходит в SSA. Accessor, absent key, dictionary и exotic receiver уходят **до** get. `OwnDataMiss` относится к точному source PC; затем этот source get использует committed lookup. Старая неудача узкого shape guard не запрещает complete lookup. Header miss запрещает повторное speculative hoisting. Effects: map words и Field(atom); вызовы и записи того же имени убивают reuse.

## Проверки

- VM unit: **1286 passed**.
- ARM64 JIT: **321 passed**, один известный baseline failure. Исполняемые probes проверяют inline/suffix, prefix bound, accessor, alias result/receiver и short chain.
- x86-64/Rosetta: **344 passed**, тот же baseline case исключён явно; новая native-проба выполнена.
- Runtime speculative loads: **17 passed**. Native proof сохраняет точное установленное поколение до и после мутации; два data reads commoned в один node. После accessor mutation два исходных get дают по одному typed miss, все 10 000 getter calls выполняются однократно.
- Property transition runtime tests: **4 passed**. Prototype validity: **4 passed**, один baseline failure.
- Test262: **1181/1181**, одинаковые totals и semantic config до/после: property accessors 21, Object.defineProperty 1131, Reflect.get 11, Reflect.set 18.
- Новая полная JS-регрессия совпала с Node в production, interpreter и при `OTTER_GC_STRESS=1`, `OTTER_GC_VERIFY=1`.
- Общий repository gate: fmt и all-features Clippy прошли; gate остановился на существующем `collecting_effects_repeat_owner_and_target_guards_without_rechecking_targets` (ожидает 2, получает 1). Отдельный snapshot restore test падает с STACK_OVERFLOW при создании donor; это воспроизведено на точном исходном source manifest. Эти сбои не объявлены исправленными.
- Дополнительная checked-сборка отменена по запросу пользователя сократить проверки; полная moving-GC матрица strides 1–16 и широкий повторный workload sweep не выполнялись.

Код применён в основном checkout. Чужие изменения сохранены. Различия между интегрированным и измеренным source вне этого среза: 2; перечень в `integrated-source-differences.json`. Оба отличия относятся только к тестам: параллельно обновлены устаревшее ожидание field guards и тест string-key atom leaf; production-код измеренного и интегрированного состояния совпадает. Следующая отдельная задача — установить причину разброса peak RSS и объём удерживаемых JIT generations; текущие данные не доказывают, что рост медианы RSS целиком вызван индексом.

## Код

- [Shape directory](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-vm/src/object/shape_keys.rs).
- [Graph IR](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-jit/src/graph/ir.rs), [builder](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-jit/src/graph/builder.rs).
- [ARM64 probe](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-jit/src/arm64/shape_lookup.rs), [x86-64 probe](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-jit/src/x86_64/shape_lookup.rs).
- [Native entry/recovery proof](/Users/alexanderstreltsov/work/octofhir/otter/crates/otter-runtime/tests/jit_speculative_property_loads/shape_directory.rs).
- [Сырые A/B результаты](/Users/alexanderstreltsov/work/octofhir/otter/benchmarks/results/ts-slice-2026-10-21/final-ab/results.json).
