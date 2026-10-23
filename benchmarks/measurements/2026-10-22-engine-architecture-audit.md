# Архитектурный аудит Otter и план крупных рефакторов

## Текущий срез — 23 октября, 02:44 UTC

Default release CLI: `629dcf4e7409eb64e69d92a4c6d6d10c178cf673484b62dd671a0281f317a4fb`. Baseline `805ec838`: `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a`. Все новые binaries, exact source patches, hashes, watchdog records и raw сохранены вне очищаемого каталога: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/`.

Финальный A/B: восемь неизменённых workload, пять чередующихся измеренных пар и одна исключённая warm-пара на класс, свежий процесс на наблюдение, production policy, CPU=user+sys, без параллельных builds/profiler. Все 96 процессов дали одинаковый stdout. CPU улучшился у Raytrace; остальные классы близки к baseline или медленнее. Общего ускорения движка этим срезом не доказано.

| Workload | CPU до → после, с | Peak RSS до → после, MiB | Wall Δ |
|---|---:|---:|---:|
| crypto | 0.20 → 0.21 | 43.81 → 45.08 | +4.36% |
| deltablue | 0.65 → 0.65 | 54.28 → 55.00 | -1.25% |
| box2d | 0.69 → 0.72 | 133.94 → 118.16 | +2.91% |
| raytrace | 0.81 → 0.76 | 46.50 → 45.55 | -6.50% |
| splay | 1.04 → 1.07 | 369.89 → 372.83 | +2.80% |
| regexp | 1.28 → 1.29 | 141.00 → 100.34 | -0.41% |
| mandreel | 1.04 → 1.06 | 222.89 → 223.95 | +2.07% |
| ts | 3.95 → 4.08 | 489.92 → 390.39 | +2.65% |

Отдельная retention workload — неизменённый TS require и два explicit full GC, три чередующиеся измеренные пары плюс warm-пара: post-GC RSS median 314195968 → 224559104 Б (299.64 → 214.16 MiB, -28.5%). Live JS 8267576 → 8255640 Б почти не изменился. Это возврат лишних физических страниц, а не сокращение живых данных. Peak RSS и post-GC RSS не смешиваются.

Реализованные архитектурные границы:

- Один heap-owned weak ShapeInterner хранит immutable geometry. Numeric writes не создают physical shape variants и не запускают migration. Property/global slots переносят один Value word напрямую.
- Int32 и purified offset-Double, включая integral Double, допустимы в любом Value. Физический Int32 tag и семантическое точное Int32 число имеют разные доказательства. Exact numeric index projection сохраняет исходный key на каждом промахе; typed scalar replacement сохраняет представление виртуальных полей на обеих ISA.
- Стартовый old-generation budget один раз учитывает реальное nursery survival. Сохраняются прежние growth bounds, headroom/cage clamp, явный embedder override и post-major formula. Это сокращает избыточную начальную promotion-driven память.
- FullGcGoal передаётся от запроса до одного sweep: natural growth/concurrent completion/stress используют Throughput; explicit GC, heap-cap и code reclamation — ReleaseMemory. Trigger/capture не выбирают policy.
- ReleaseMemory возвращает ОС целые внутренние OS pages в неиспользуемых nursery payload/tails и в существующих old-space dead ranges/tails. Реальный OS page size, live prefixes, page/FREE headers и intrusive links сохраняются; addresses/capacity/free-list facts не меняются. Unsupported/failure path не трогает unused bytes.
- GeneratedCodeBytes учитывает принадлежащие deopt/safepoint metadata actual-argument arrays. Исправление accounting не объявляется экономией RSS.

Разделение memory-release/throughput и сохранение FreeSpace prefix сверены с pinned V8 `926c0e5…`, `src/heap/sweeper.cc:1008–1063`, `memory-reducer.cc:58–94`. Exact primary sites и локальный аудит: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/gc-collection-goal/v8-policy-audit.txt`.

Проверки: 135 focused numeric cases на ARM64/x86-64 и 7 адресных случаев после FLOAT saturation; 17 GC/accounting cases; дополнительная проверка active/inactive nursery tails с capture off/on; 32 CLI differential/GC observations (interpreter/jitless/production, affected stress 1/4/16). Targeted Test262 closing gate: все 171 canonical file прошли, без skips/failed/crash/timeout/OOM (Number.isInteger 9, Object.is 22, unary-minus 14, String.replace 56, RegExp @@replace 70). Runner SHA-256 `b9058e97b8f8b90946b2cb9a8d613fbca676a3e980ca712b64c78eeb4488b5fa`; reports и exact pinned selection: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/targeted-test262/final-run/`. Это не полный свежий conformance report; исторический `ES_CONFORMANCE.md` не переписывался.

Отклонённый CPU-кандидат: прямой pristine static-string replace entry прошёл source review, но новый stress unit дал literal `$<part>` вместо named substitution при доказанном pristine receiver. Production-entry и его новые fixture edits полностью откатили, failure logs/patch сохранены в `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/regexp-pristine-entry/`. Причина не установлена; переносом теста её не скрывали.

Неподтверждённые объяснения отклонены: установленный код не удерживает целиком Graph/regalloc/snapshot/AST, dense TS feedback даёт upper bound около 1.82 MiB; ShapeInterner weak prune вызывается ровно один раз на physical sweep. Vmmap нашёл ~152 MiB mimalloc mappings вне GC cage, но не различает живые Rust owners и allocator fragmentation. Нельзя объявлять это утечкой или приписывать одному компоненту.

Дальнейший крупный CPU-план: исследовать Box2D Number-union loads и GC startup cost Splay по actual installed code/cycles; для следующего шага code-GC сначала доказать inactive-generation/active-frame ownership. Не возвращать numeric migrations, per-read canonicalization, второй interner или несовместимые parallel stacks.


### История поиска числовой границы до 01:32 UTC

Принятый baseline — `805ec838`, executable SHA-256 `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a`. Новые raw находятся в `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/`. Более ранние потерянные raw остаются потерянными.

Физические numeric shapes и затем geometry-only fields с canonicalization каждого чтения отвергнуты по whole-run CPU. Последний `3ce990bd` прошёл 106 адресных tests на двух ISA и 23 CLI observations, но RayTrace CPU вырос 0,79 → 0,91 с, TS 3,97 → 4,38 с. Он не принят в Git. Текущий working tree закрывает одну числовую границу целиком:

- один heap-owned weak `ShapeInterner` владеет геометрией; numeric writes не создают physical shape variants;
- любой `Value` может содержать Int32 либо purified offset-Double, включая integral Double; `as_i32` доказывает только физический Int32 tag;
- property/global reads переносят word без canonicalization; F64 boxing только очищает NaN и добавляет offset;
- shape-owned `FieldType` различает Int32, Double и Number union, без второго representation registry;
- финальный выбор store observers использует фактически записываемое слово после representation/escape/folding; semantic Int32 knowledge не выдаётся за raw Int32 proof;
- scalar replacement сохраняет тип виртуального поля, поддерживает Float64 store/Number load/Tagged reconstruction; наблюдаемые записи остаются физическими;
- старые preferences, representation banks/dependencies, migrations и birth packets удалены. Canonical births используют один буфер и раздельные GC roots.

```mermaid
flowchart LR
    S["ShapeInterner: геометрия"] --> P["Property field: Value word"]
    F["F64 SSA"] --> E["Purify NaN + offset"] --> P
    V["Int32 / Double Value"] --> P
    T["FieldType: I32 / Double / union"] --> N["Числовое чтение"]
    P --> N
    P --> B["Прямое Tagged чтение: регистры, args, globals"]
    F --> Q["Typed virtual field: scalar replacement"]
```

JSC `compilePutByOffset` пишет purified offset-Double напрямую (`DFGSpeculativeJIT.cpp:14876–14905`, ревизия `e9c03b…`). Его JSValue bit equality/hash остаются физическими; JS strict equality/SameValue имеют отдельную numeric семантику. Otter сохраняет это разделение. Узкий JSC `unboxRealNumberDouble` не переносится как полный Number decoder: он исключает Double NaN, тогда как Otter numeric field contract допускает NaN и ±Infinity.

Universal-Value release `8ac95d00` прошёл 119 адресных tests, 32 CLI observations и 64 whole-run observations. RayTrace CPU улучшился 0,83 → 0,74 с, TS вернулся к baseline, но crypto вырос 0,21 → 0,90 с, Box2D 0,73 → 0,93 с. Этот вариант также отвергнут. Exact crypto profile нашёл потерянную числовую границу: StoreKeyedCached принимает только raw Int32 key, а цикл производит integral Double. На runtime completion приходится 451/892 isolate inclusive samples. Текущий срез добавляет отдельную exact numeric-index projection с сохранением исходного Value на промахах, на обеих ISA. VM использует существующий `array::index_from_number`; raw Int32 proofs не расширяются. Index release `0d81a354` убрал основную crypto-регрессию, но сохранил CPU 0,20 → 0,37 с; Box2D 0,70 → 0,90 с. После этого исправлена сама граница numeric feedback: точный целый Double, кроме `-0`, допускает Int32 SSA. `as_i32`/FieldType остаются физическими; semantic backlink больше не выдаёт physical INT32 bound. Release `9b275330` прошёл 135 адресных cases и 32 CLI observations; crypto вернулся к baseline, RayTrace CPU улучшился примерно на 10%, остальные классы ещё имеют небольшие регрессии. Следующий вариант пропускает повторную FP-классификацию, когда существующий numeric feedback cell уже содержит FLOAT; новый release ещё не измерен. Дополнительно подтверждён GC resource target: после полного GC неизменённый Splay удерживает около 260 МиБ old extent при 62 МиБ занятых ячеек. Свободные промежутки переиспользуются между GC; это не доказательство потери freelist. Проверяется возврат OS pages внутри мёртвых диапазонов без перемещения живых объектов.

> Состояние артефактов, 22 октября 2026, около 20:15 UTC: во время текущих проверок исчезли созданные в этой сессии каталоги `benchmarks/results/*2026-10-21*` и `*2026-10-22*`, включая raw A/B, patches и сохранённые baseline executables. Причина не установлена; команды этого чата удаляли только Cargo build caches. Числа ниже остаются исторически наблюдёнными результатами, но соответствующие старые raw-ссылки сейчас недоступны и не являются воспроизводимым набором доказательств. Исходники и Git-коммиты целы. Новые records/executables сохраняются в `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/`; baseline текущего среза требуется пересобрать из коммита, а адресные gates повторить для восстановления записей.

## Текущий план по конкретным механизмам V8 и JavaScriptCore

Исходники доступны локально и проверены на точных ревизиях: V8 `926c0e5fb9cd60257e49cbdf7855ba42dd13963c`, WebKit/JSC `e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e`. Ниже указаны действительные механизмы и границы переноса. Полный перенос чужого коллектора или оптимизатора не требуется.

| Граница | Что действительно делает другой движок | Разница Otter и действие |
|---|---|---|
| Ссылки установленного native code | V8 сохраняет обязательные relocations фактически выпущенного кода; deopt literals формируются отдельно из translation operands | Otter сохранял весь prepared root set, включая отвергнутых inline-кандидатов. Реализован mandatory cell-use proof из emission плюс владельцы finalized dependency metadata; один `CompilationRoot` owner после установки |
| Фоновая компиляция | V8 удаляет queued jobs и передаёт активным atomic cancellation flag; compiler проверяет его между фазами; source handles живут до завершения job | Реализован общий worker-visible token, terminal cancellation, удержание исходных chunks до ACK и проверка фактически spliced sources перед publication |
| Жизнь неактивного optimizing code | JSC делает связь Executable → optimizing CodeBlock условной; speculative references слабы, активный/marked CodeBlock удерживает их сильно | Используемые константы Otter пока сильны весь срок generation. Это отдельный будущий code-GC срез; обнулить ссылки без retirement/active-frame протокола нельзя |

### 1. Установленный код получает свои реальные ссылки

В V8 `MacroAssembler::Cmp` регистрирует embedded object при выпуске инструкции; `CodeBuilder` принимает `CodeDesc`, переносит relocations в `InstructionStream`, а GC обходит этот обязательный список. Это runtime metadata, существующее без diagnostic capture. Отдельный deopt literal table строится из translation operands и дедуплицируется: перечень machine references не заменяет recovery metadata. [Эмиссия](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/codegen/x64/macro-assembler-x64.cc#L3235), [перенос relocations](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/instruction-stream-inl.h#L134), [GC обход](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/objects-body-descriptors-inl.h#L1757), [deopt literals](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/backend/code-generator.cc#L1259).

Otter воспроизводит конкретный дефект владения: `cold` вызывается один раз и захватывает массив из 1 048 576 элементов; `hot` получает настоящий Graph OSR. Sloppy `cold` наблюдает `this`, поэтому plain-call с `undefined` не допускает splice. Events подтверждают prepared candidate, artifacts — отсутствие actual inline и отсутствие отдельного native body `cold`. После возврата и двух GC payload case удерживает **9 656 416 Б**, контроль с восемью элементами **1 136 808 Б**. Разница **8 519 608 Б** принадлежит подготовленным, но не использованным владельцам. Сырые source/events/artifacts: `benchmarks/results/root-use-2026-10-22/before-v2-*`. Первый строгий вариант repro действительно inlined callee и потому не доказывал дефект; его данные сохранены отдельно и не используются как доказательство unused owner.

Исправление завершает одну границу producer → consumer: оба native emitter регистрируют фактические stable-cell references независимо от `--jit-artifacts`; Graph добавляет strong context owners, необходимые finalized `ClosureContext` dependencies даже после устранения context load; `JitCompileStatus::Compiled` переносит sorted unique `required_cells`; VM при публикации фильтрует только `CalleeIdentity` roots. Pending compilation сохраняет весь подготовленный набор до terminal result. Shape/family/prototype/instanceof ownership этим срезом не меняется. В установленном code object второй reference registry не появляется. Проверки должны доказать освобождение unused payload и сохранность действительно используемых native constants через moving GC на обеих ISA.

### Измеренный результат pruning установленных roots

Baseline `857a9cdb7`, SHA256 `0a4e85804f81e7e33169b66fc14cfde725424aad810a05b9d11d084e6a73f4c7`; after SHA256 `1a33806f2e343a534078f4040823e8ff7c4915397b38595834c48a60b7a44d21`. After включает также отдельный COW bounds fix `26dbe8ae2`. Оба executable собраны отдельно с одинаковой default production конфигурацией. Rust builds/profiler не работали во время измерений; host snapshots сохранены. Три чередующиеся AB/BA-пары после одной исключённой warmup-пары на каждом классе; все 64 процесса завершились с одинаковыми результатами. CPU — median суммы user+sys каждого процесса, включая worker threads.

| Класс | Wall median, с до → после | Δ wall | CPU median, с до → после | Peak RSS median, МиБ до → после |
|---|---:|---:|---:|---:|
| crypto | 0.2042 → 0.2049 | +0.34% | 0.21 → 0.21 | 43.80 → 43.41 |
| deltablue | 0.6555 → 0.6588 | +0.50% | 0.68 → 0.67 | 60.38 → 61.33 |
| box2d | 0.6119 → 0.6314 | +3.17% | 0.70 → 0.74 | 132.66 → 135.03 |
| raytrace | 0.8139 → 0.8523 | +4.72% | 0.82 → 0.84 | 46.77 → 47.20 |
| splay | 0.6371 → 0.6584 | +3.34% | 1.18 → 1.20 | 381.12 → 381.02 |
| regexp | 1.3544 → 1.3840 | +2.18% | 1.34 → 1.35 | 140.52 → 141.05 |
| mandreel | 0.9995 → 1.0098 | +1.03% | 1.06 → 1.06 | 224.12 → 223.61 |
| ts | 3.0864 → 3.0626 | -0.77% | 4.13 → 4.11 | 495.64 → 501.52 |

| Класс | Wall min…max до / после, с | Peak RSS min…max до / после, МиБ |
|---|---:|---:|
| crypto | 0.204…0.206 / 0.205…0.205 | 43.391…44.766 / 42.719…45.688 |
| deltablue | 0.649…0.665 / 0.639…0.659 | 57.797…60.906 / 60.172…62.422 |
| box2d | 0.611…0.629 / 0.616…0.651 | 132.656…136.859 / 130.797…136.828 |
| raytrace | 0.807…0.903 / 0.832…0.853 | 46.406…47.422 / 45.781…47.359 |
| splay | 0.637…0.643 / 0.634…0.669 | 380.828…385.547 / 376.688…383.375 |
| regexp | 1.334…1.358 / 1.322…2.087 | 139.359…140.969 / 139.516…141.078 |
| mandreel | 0.996…1.018 / 1.004…1.012 | 219.500…226.625 / 218.641…225.078 |
| ts | 3.065…3.095 / 3.040…3.089 | 488.906…500.656 / 500.516…501.766 |

Общего ускорения нет: TS wall −0,77%/CPU 4,13 → 4,11 с, но остальные medians wall выше на 0,34…4,72%; Box2D CPU 0,70 → 0,74 с. Это результат владения памятью, не достигнутая цель speed+resources. В двух зафиксированных hot-code артефактах до/после codeBytes совпадают (Template 5400, Graph 2536); инструкциям не приписывается speedup.

Три адресные post-GC пары: payload heap **9 656 416 → 1 136 584 Б (−88,23%)**, control **1 136 808 → 1 136 584 Б**; все три значения внутри каждого набора совпали. Payload RSS median **34,50 → 26,30 МиБ**. Actual `hot` получил Graph OSR, `cold` не имеет собственного native body и не был spliced; наличие prepared candidate и отсутствие `inlineLowered` сохранены в `native-use-proof.json`. Старый production executable падает на новой retention assertion; interpreter и новый executable проходят.

На прежнем неизменённом require-wrapper полного TS три новые пары дали heap до **7,885…48,800 МиБ**, после **7,885…7,885 МиБ**; post-GC RSS median **301,53 → 300,89 МиБ**. Эти три прогона не доказывают отсутствие остаточного скачка во всех расписаниях background compiler. Wrapper retention не смешивается с plain-fixture peak RSS в таблице выше.

Адресные gates: mandatory sorted use proof и metadata-only accepted contexts, actual cell emission с выключенным artifact capture — по 4 JIT unit tests на ISA; existing VM owner filter 2; actual native context/closure — по 2 на ISA; shared Tagged growth moving-GC fixture — по 2 на ISA. CLI driver: 32 успешных процесса и один ожидаемый baseline failure, включая interpreter oracle / production / jitless / interpreter для двух новых corpus и existing unique-context, затронутые GC strides 1/4/16, retention и actual artifacts; ещё шесть TS retention процессов. Release ENOSPC исправлен удалением только пересоздаваемых debug incremental caches; failed build log сохранён, тесты не перезапускались.

Все команды, точный compiled-source patch, source/binary hashes, build failure/retry, диапазоны и сырые stdout/time: `benchmarks/results/root-use-2026-10-22/`.

### 2. Worker должен прекращать ненужную работу

Это готовая модель V8, а не предложение убивать потоки. Dispatcher `FlushJobsForIsolate` удаляет queued jobs; `CancelCompilationJobsForIsolate` вызывает `job->Cancel` у активных jobs; atomic flag проверяется до graph и после каждой compiler phase. Teardown сначала flush/cancel, затем ждёт jobs и их LocalHeap destruction, затем освобождает output без установки. [Queued/active lifecycle](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler-dispatcher/optimizing-compile-dispatcher.cc#L247), [проверка между фазами](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/pipeline.cc#L839), [удержание source](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/codegen/optimized-compilation-info.h#L312).

Перед publication V8 дополнительно проверяет detached context, disabled optimization, deprecated maps и dependency validity; commit повторяет validation непосредственно перед installation. Otter уже проверяет dependencies при install — это существующее совпадение. Добавленные границы: worker-visible cancellation и source lifetime при eviction (`Arc<CodeBlock>` не является lease на независимо evictable `ChunkPayload`). [Dependency commit](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/compilation-dependencies.cc#L1461).

Реализация: один monotonic atomic token Pending/Job/Request; queued cancellation удаляет request из FIFO, уничтожает snapshot вне mutex и публикует ACK без вызова compiler hook. Active compiler проверяет token до/после analysis, plan, Graph construction, отдельных optimization passes, register allocation, target emission и final metadata/artifact output. Текущая фаза может завершиться целиком: millisecond deadline и остановка внутри builder/regalloc не обещаются. Проверки до/после hook и перед publication также отклоняют поздний ответ стороннего hook, игнорирующего token. Отмена имеет terminal `cancelled`, не `Unsupported`; queued skip имеет `compileStartedNs: null` и duration=0. Compile attempts учитывают фактически вошедшие hooks.

Pending сохраняет `CompilationSources` с физическими `ExecutionContext` owners всех подготовленных plain/mono/poly/direct-method bodies до terminal ACK. Isolate-side owner не передаётся worker. Final admission независимо проверяет root CodeBlock/CodeSpace и owners фактически spliced functions. Pressure census сначала отменяет requests и освобождает готовые ACK; ещё активный job удерживает chunk, затем его ACK повторно разрешает обычную проверку pressure watermark. ACK сам не запускает GC. Teardown отменяет и присоединяет worker threads до освобождения heap/shape runtime и pending owners.

Первый production прототип выявил реальную регрессию: source traversal пропустил четвёртую ветвь `direct_methods[*].body`. Artifact с таким фактическим splice отвергался; ошибочная фильтрация compile-attempt counter оставляла бюджет немедленно готовым к следующей компиляции. DeltaBlue warmup вырос **0,6956 → 167,1174 с**; следующий процесс остановлен после sample. Матрица имеет явный `aborted-performance-regression` и не используется как итоговый A/B. Исправлены обе границы: source closure включает direct methods, а текущий root с неверным splice получает обычный `installDeclined: invalidCode`, учитывает попытку и кэширует отказ на том же feedback. Только dead/foreign root становится cancellation, без воскрешения per-function состояния. Новый regression проверяет отсутствие повторного hook после invalid splice.

Короткие gates: пять VM/JIT unit tests по cancellation (включая существующий host-job case), source lease test с пятью независимыми chunks и foreign CodeSpace с совпадающими ids, queued/active ACK ownership и ignored-token publication refusal; actual native optimizing OSR source-work regression; 32 успешных CLI процесса с production/jitless/interpreter oracle, GC strides 1/4/16, retained-root control/payload и actual Graph artifacts. ISA encoders этим срезом не менялись. Два пропущенных test caller аргумента найдены первым cold build и исправлены; failure log сохранён.

Полная доля wasted compilation в workload CPU **ещё не измерена**. Подготовленный embedded `compile-waste-2026-10-22/drop_tail.rs` не дал сравнимых до/после данных: первый link отказал из-за LTO, затем по запросу пользователя выполнен `cargo clean` (70 204 файлов, 28,1 GiB). Сумма compiler wall durations не называется CPU хвоста. Сохранённые production executables и сырые результаты clean не затронул.

Финальный default production CLI собран после ремонта, SHA256 `4884daf5e04dd5358efde5b6ebaa7f0e97adc94a3a183cd72ee3395a1eb9650e`; baseline предыдущего среза `1a33806f2e343a534078f4040823e8ff7c4915397b38595834c48a60b7a44d21`. Три AB/BA-пары плюс исключённая warmup-пара на каждом из восьми классов завершили все 64 процесса с одинаковыми результатами. Однако в середине матрицы началась **внешняя сборка `server-rs`**, зафиксированная в `regexp-0..3-processes.txt` (Cargo, затем несколько rustc около 80–99% CPU). Наши builds/profiler во время A/B не запускались. Эта матрица помечена `performanceInterpretation: contaminated-external-build`; она не доказывает ни ускорение, ни отсутствие регрессии. Ниже сохранены наблюдения, без принятия их за performance score.

| Класс | Wall median, с до → после | CPU median, с до → после | Peak RSS median, МиБ до → после |
|---|---:|---:|---:|
| crypto | 0.2176 → 0.2326 | 0.22 → 0.25 | 43.11 → 44.14 |
| deltablue | 0.7550 → 0.6602 | 0.74 → 0.69 | 61.36 → 61.34 |
| box2d | 0.6511 → 0.6587 | 0.74 → 0.74 | 132.28 → 133.86 |
| raytrace | 0.9163 → 0.9861 | 0.90 → 0.90 | 46.92 → 46.72 |
| splay | 0.7295 → 0.6903 | 1.23 → 1.20 | 365.95 → 366.80 |
| regexp | 1.8271 → 2.6008 | 1.74 → 2.25 | 139.52 → 140.11 |
| mandreel | 1.3604 → 1.0464 | 1.40 → 1.10 | 224.08 → 221.88 |
| ts | 3.6456 → 3.9092 | 4.70 → 5.10 | 496.78 → 495.64 |

Все min/max каждого wall/CPU/RSS и исходные per-process counters находятся в `repair/whole-ab/summary-cpu.json` и `results.json`. В частности regexp wall до 1,686…2,724 с / после 2,558…2,890 с; executed instructions практически совпадают (18,56…18,62 / 18,61…18,70 млрд), чего недостаточно для вывода об одинаковой скорости на ненагруженном host.

После окончания внешней сборки выполнена новая полная матрица `repair/whole-ab-repeat/`: ещё 64 процесса, три измеряемые пары на класс, hashes неизменны, outputs совпадают; snapshots не содержат Cargo/rustc. Обычная UI/system нагрузка host остаётся. Crypto wall 0,2546 → 0,2276 с при CPU 0,23 → 0,24; DeltaBlue 0,6486 → 0,6456 / CPU 0,67 → 0,67; Box2D 0,6085 → 0,6190 / 0,69 → 0,70; RayTrace 0,8403 → 0,9012 / 0,83 → 0,89; Splay 0,6201 → 0,6289 / 1,06 → 1,09; RegExp 1,3210 → 1,3582 / 1,34 → 1,34; Mandreel 0,9896 → 1,0103 / 1,05 → 1,06; TS 3,1809 → 3,3166 / 4,22 → 4,34. Полные RSS/instructions/min/max находятся в новом `summary-cpu.json`. **Общего ускорения нет**; этот срез завершает cancellation/source-lifetime границу, а цель speed+resources остаётся открытой.

Ещё три чередующиеся пары неизменённого TS post-GC wrapper: live JS heap до **7,884781…7,884811 МиБ**, после **7,884811…7,884964 МиБ**; RSS median **299,44 → 305,34 МиБ**, диапазоны 287,11…302,09 / 301,61…305,73 МиБ. Это не выигрыш памяти, а проверка отсутствия прежнего крупного retained JS payload на этих расписаниях. Wrapper не заменяет plain fixture peak RSS.

Первый дефектный executable, прерванная матрица, sample compile storm, repaired source patch, hashes, cold build failures, команды и сырые gates: `benchmarks/results/compile-cancellation-2026-10-22/`; исправленный финальный набор — `repair/`.

### Законченный срез: один владелец подготовленного method target

До этого рефактора `jit_compile.rs` передавал `direct_methods[*].body.clone()` в legacy method baking; `jit_inline_methods.rs` повторно сохранял тот же Arc в mono **или** poly таблице. Graph читал direct carrier, Template — legacy carrier. Затем `jit_field_types.rs` вызывал `Arc::make_mut` отдельно для обеих ветвей: sharing превращался в две owned snapshot copies на overlap edge, включая instruction overlays и вложенные maps. Цепочка трёх таких edges допускала восемь leaf copies и 14 descendant snapshots вместо трёх различных descendants. Inline bytecode budget оплачивал `bake_inline_body`, но не эти copy-on-write copies. Это статически доказанный механизм; прежние events показывают присутствие обеих групп на DeltaBlue, но не позволяют честно вычислить runtime bytes или процент CPU.

Реализована одна таблица guarded method targets, один body на `(site, target)`, optional direct call plan и optional Template leaf field proof. Удалены три прежние карты и два carrier types, producer и оба tier consumers переведены одновременно. Field/context/source scans проходят одно дерево. Template leaf planning теперь читает snapshot напрямую: прежний полный clone с последующим clear target maps не нужен, поскольку plan builder их не использует. Target без call plan необходим: Template умеет inline body при отсутствии entry generation; Graph сохраняет требование call capability. Глобальная дедупликация только по function id неверна, поскольку guards и context constants относятся к конкретному target. Chain indices сохраняют исходную позицию до capability filtering на обеих ISA.

Budget policy теперь одна: Graph-capable method body готовится один раз с фактической site frequency; отказ больше не повторяется вторым frequency-free leaf request. Template-only body сохраняет прежнюю leaf admission. Это намеренное изменение admission, требующее полного A/B, а не обещание идентичного выбора всех inline candidates.

Реальный VM producer regression создаёт verified root → три nested methods, живые shaped descriptors/PIC и опубликованные entry cells. Он наблюдает 3/6 уникальных body owners для mono/poly случая; каждый Arc имеет strong count=1. После `bake_field_type_tree` адреса Arc и instruction buffers совпадают, реальный leaf field bound заполнен; budget расходуется ровно по одному разу на body. Одинаковый function id на двух receiver shapes сохраняет отдельные targets/field locations. Native unit regressions проверяют отсутствие Graph admission у Template-only target и исходные индексы после его пропуска. Дополнительный JS corpus проверяет разные captured contexts одного method source, prototype replacement, method getter и own-field accessor; старый production и interpreter дают одинаковый результат, поэтому существовавшее runtime-падение ему не приписывается.

Завершён в `805ec8380960a48d83a1fba4094fee60ae9d56fd`. После потери старого каталога повторно сохранены адресные gates: x86_64 JIT method 11, ARM64 VM method-owner 2, source lease 1, ARM64 JIT method 10, runtime method 11, descriptor/accessor 1 и method artifact 1 — всего 37 tests. CLI driver сохранил 29 успешных процессов: четыре затронутых corpus против interpreter, production/jitless/interpreter, GC strides 1/4/16 и отдельный native events proof. Native context case получил entry и OSR optimizing compilations и 11 фактических inline splices. Полные suites не запускались.

Production `after-otter` SHA256 `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a` сохранён вне каталога результатов. Предыдущий source `2995db95596f3b76ebb70a78a4d54591b04cd3f6` пересобран в том же checkout командой `cargo build --release -p otter-cli` без profile/env overrides; получен baseline SHA256 `352e9c31dd69ce0d17e5f225cd7634e323a9fc468ac59a0eae4b71f1350dd3e8`. Затем `main` восстановлен на тот же чистый source commit. Дополнительных source checkouts/copies не создавалось.

Три чередующиеся AB/BA-пары плюс исключённая warmup-пара для каждого из восьми неизменённых классов: все 64 процесса дали одинаковые результаты. Process snapshots не содержат Cargo/rustc/profiler; обычная UI/system host load остаётся. CPU — median от user+sys каждого процесса, RSS — peak.

| Класс | Wall median, с до → после | Δ wall | CPU median, с до → после | Peak RSS median, МиБ до → после |
|---|---:|---:|---:|---:|
| crypto | 0.2029 → 0.2021 | -0.40% | 0.20 → 0.20 | 43.44 → 44.14 |
| deltablue | 0.6349 → 0.6460 | +1.74% | 0.66 → 0.66 | 61.95 → 53.36 |
| box2d | 0.7548 → 0.6949 | -7.94% | 0.84 → 0.77 | 133.39 → 133.69 |
| raytrace | 0.7993 → 0.7860 | -1.67% | 0.80 → 0.79 | 46.64 → 46.28 |
| splay | 0.6344 → 0.6054 | -4.56% | 1.05 → 1.02 | 371.55 → 371.41 |
| regexp | 1.2998 → 1.2829 | -1.30% | 1.31 → 1.29 | 140.77 → 140.84 |
| mandreel | 0.9869 → 1.0010 | +1.44% | 1.04 → 1.05 | 224.41 → 222.73 |
| ts | 3.0587 → 3.0603 | +0.05% | 4.02 → 4.07 | 488.09 → 496.86 |

| Класс | Wall min…max до / после, с | RSS min…max до / после, МиБ |
|---|---:|---:|
| crypto | 0.2005…0.2054 / 0.2003…0.2024 | 42.80…43.64 / 44.02…44.97 |
| deltablue | 0.6337…0.6411 / 0.6198…0.6561 | 58.59…62.58 / 52.39…56.22 |
| box2d | 0.5977…1.0618 / 0.6098…0.8487 | 132.70…135.81 / 130.44…134.69 |
| raytrace | 0.7834…1.1694 / 0.7614…1.0591 | 46.42…46.67 / 46.23…47.58 |
| splay | 0.6308…0.6373 / 0.6009…0.6083 | 365.06…372.69 / 368.66…375.97 |
| regexp | 1.2777…1.4486 / 1.2689…1.2961 | 140.03…141.08 / 139.11…140.86 |
| mandreel | 0.9802…1.0104 / 0.9986…1.0132 | 223.36…226.45 / 220.58…225.81 |
| ts | 3.0187…3.1574 / 3.0280…3.3164 | 487.20…490.66 / 494.23…499.12 |

**Общего выигрыша speed+memory нет.** Splay wall −4,56% при CPU 1,05 → 1,02 с; Box2D median −7,94%, но диапазон велик и перекрывается. TS wall практически одинаков, CPU +1,24% и RSS +1,80%; Mandreel немного хуже. Это устранение доказанного copy-on-write fan-out и повторного planning, а не достигнутая цель крупного ускорения. Стоимость snapshot preparation отдельно по CPU не измерена, RSS нельзя приписать только этому владельцу. Следующий execution срез меняет физический формат числовых полей.

Устаревшие exact artifact assertions (5 registers/5 opcodes) до потери executable проверены baseline CLI: он уже выдавал 4 registers/4 opcodes и slots `[0,1,0,null]`; тест обновлён к этой действующей форме, без ослабления range/ownership checks. Старая запись проверки недоступна; повторный текущий artifact gate сохранён.

Новые исходные команды, snapshots, raw stdout/time, source/binary hashes, `baseline-build.json`, `gates.json`, `diagnostics.json`, `whole-ab/results.json`, executables и source patch: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/method-targets/`. Эти records не восстанавливают утраченные ранние matrices.

### Следующий срез: числовой формат поля и один shape interner

Выбран `DoubleEncodedOrUndefined` как immutable physical field codec рядом с обычным Tagged. Int32/undefined в обычных полях сохраняют прежнюю кодировку; только выбранное Double-поле нормализует числовые записи. Undefined остаётся точным sentinel, поэтому numeric load проверяет инициализацию до F64 decode. На границе обычного Value read/deopt число возвращается в canonical Number. Нечисловая запись переводит lineage preference в absorbing General до эффекта; native probe обязан отказать до store. Полностью сменить глобальный number boxing или применить только Graph store недостаточно — прежние такие эксперименты дали регрессии.

Neutral defining shape владеет монотонной preference Candidate → Double → General, physical shapes остаются immutable. Все поля repair/birth переигрываются по одному предпочтительному вектору, чтобы порядок переходов сходился к одному layout. Старые Graph constructor/literal plans подписываются на изменение preference через существующие code dependencies; следующий объект должен рождаться с актуальным codec. Field summary описывает фактические physical words определяющего shape, а не neutral origin. No second registry, compatibility mode или новая схема artifact format.

Первый необходимый foundation — одна heap-owned таблица переходов. Сейчас interpreter facade и heap-only state transitions не имеют общей mutable authority. `IsolateNames` получает существующий interner; producers, restore, root pins и GC weak pruning переводятся одновременно. Nonalloc table borrow никогда не пересекает allocator или root-publication reentry. Weak sweep выполняется до физического освобождения, включая split mark/sweep. Этот foundation сам по себе не объявляется performance result; он задаёт единственного владельца для следующего physical codec.

### Первый полный field-codec вариант: не принят, модель уточняется

Production CLI SHA256 `5bf51dfde1c6bed4bbf90257ff5a84dd9f94733510110194099b2878af833dc4` собран отдельно default `cargo build --release -p otter-cli`, без profile/env overrides. Baseline — предыдущий `805ec838` executable `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a`. Exact source patch, build manifest, binaries и все новые raw records сохранены вне очищенного дерева: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/shape-interner/`.

Полная реализация уже имеет общий heap interner, физические codecs и оба native backend, точное admission до store/barrier/summary, canonical boxed boundaries, предпочтительные births и retirement stale Graph birth plans. Удалён прежний отдельный `field_repr` registry и неиспользуемый альтернативный atom-store owner. Носители не растут: ShapeBody 96 B, FieldLocation 4 B, ShapeKeys entry 16 B, PIC entry 32 B, shared property row 72 B; поля остаются **8 B**, поэтому уменьшение object footprint не заявляется. Дополнительные historical variants, compilation facts и dependencies имеют реальную цену. Два birth обхода сделаны линейными; same-neutral-prefix write не повторяет наблюдение всех старых значений. Это не substitutes для whole-process результата.

Адресные gates первого варианта: heap-interner 4; representation/owned-layout 3; VM `double_` 6, включая реальное перемещение receiver и incoming object на overflow allocation; обе ISA codec по 2; ARM property actions 2 + typed fields 1 + literals 11; x86 actions 4 + typed fields 1 + literals 8; настоящий Runtime Graph birth invalidation по 1 на ISA. CLI сохранил 16 успешных процессов: два corpus против старого interpreter, текущие interpreter/Template/production и production GC strides 1/4/16; numeric corpus дополнительно проверен при stride 1 в interpreter и Template. Полные suites не запускались.

A/B остановлен на **42-м процессе из 64**: unchanged RegExp завершился TypeError в excluded warmup after. Поэтому это незавершённая, failed матрица, не общий performance score. Пять законченных классов до ошибки имеют три AB/BA пары и excluded warm pair; Rust builds/profiler не запускались вместе с ними. Обычная system/UI нагрузка сохраняется в process snapshots.

| Класс | Wall median, с | Δ wall | CPU median, с | Peak RSS median, МиБ | Wall min…max до / после, с |
|---|---:|---:|---:|---:|---:|
| crypto | 0.2108 → 0.2042 | -3.14% | 0.21 → 0.21 | 42.89 → 43.70 | 0.202…0.215 / 0.203…0.215 |
| deltablue | 0.6347 → 0.6505 | +2.48% | 0.65 → 0.67 | 53.28 → 53.89 | 0.621…0.745 / 0.644…0.653 |
| box2d | 0.6166 → 0.9324 | +51.22% | 0.70 → 1.05 | 131.83 → 138.42 | 0.602…0.627 / 0.918…0.959 |
| raytrace | 0.8080 → 1.1438 | +41.56% | 0.81 → 1.14 | 46.86 → 45.58 | 0.790…0.838 / 1.091…1.237 |
| splay | 0.5966 → 0.6352 | +6.47% | 1.00 → 1.04 | 374.27 → 370.77 | 0.590…0.598 / 0.612…0.646 |

RegExp ошибка воспроизводится во всех трёх tier modes, включая `/a/.exec("a")`: folded builder входил напрямую в no-collect allocation и не готовил первый String `input` через общий birth owner. Исправление вызывает тот же `field_storage::prepare_birth` до reservation и удерживает actual physical shape/record roots; новый адресный VM regression первого input и следующего рождения после `match.index = 1.5` прошёл. Исправленный release ещё не измерен на этом этапе.

Box2D — существенная регрессия, а не noise: executed instructions median 9.573 → 14.079 млрд. Полные статические artifacts показывают, что numeric lowering работает: в latest emitted body per function есть 1 021 Double load/226 Double stores; CheckedTaggedToFloat64 1 246 → 394. Но optimizing code вырос с 1 440 456 до 2 116 696 B, главным образом за счёт cached/named property paths. В f1217 прежний 4-B LoadOwnField после ShapeGuard превратился в 604-B cached operation; runtime store counters этой функции 23 → 17 920. Исторические Tagged и Double PIC cases мешают общей direct-field admission. Captured ShapeGuard exits 10 → 31, TypeMismatch 3 → 2: гипотеза неограниченного TypeMismatch churn этими данными **не подтверждается**. Event capture bounded/truncated 16 384; counts — только захваченный prefix, latest emitted artifact не доказывает active generation. Отдельный launchprof дал 641/926 isolate self samples, из них optimizing 398/613; это один диагностический профиль, не A/B score.

Следующая корректировка переносит адаптацию старых физических объектов в один noncollecting representation migration owner: exact neutral origin и полный текущий preferred vector, без изменения observable JS values. Own-field compile projection нормализует исторические equivalent layouts к текущему physical target; native shape guard мигрирует только на cold mismatch, а generation подписывается на preferences **до** shape check. Unrelated polymorphism не стирается и не переучивается безусловно. В V8 этому соответствует `TryMigrateInstance` на mismatch с повторной проверкой map и field-representation dependency на defining owner ([выбор check kind](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/property-access-builder.cc#L109), [migration/recheck](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/compiler/turboshaft/machine-lowering-reducer-inl.h#L3564), [invalidation](https://github.com/v8/v8/blob/926c0e5fb9cd60257e49cbdf7855ba42dd13963c/src/objects/map-updater.cc#L1391)). Это адаптация механизма; deprecated V8 maps не объявляются уже существовавшим контрактом Otter. Результат корректировки пока не измерен.

Отвергнут отдельный кандидат frame-state pruning: `graph/builder.rs` уже берёт live_at(pc)/live_at(next), parent inline frames сохраняют нужное after-call state, regalloc/deopt потребляют сокращённые states. V8 `BytecodeGraphBuilder::PrepareEagerCheckpoint` на pinned revision также использует in-liveness и sparse StateValues. Повторно внедрять тот же механизм не требуется.


### 3. JSC: weak code dependencies и активный стек

JSC — более широкий механизм, чем текущий Otter cell pruning. `Graph::freeze` по умолчанию создаёт `WeakValue`; `handleGetScope` специализирует function scope через `weakJSConstant`. При linking `registerFrozenValues` обходит **все** frozen values, а не только emitted uses: weak values переходят в `DesiredWeakReferences`, strong — в CodeBlock constants. Поэтому приписывать JSC наш точный emitted-use алгоритм было бы неправильно. [Scope constant](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGByteCodeParser.cpp#L7707), [registerFrozenValues](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGGraph.cpp#L1664).

Пока compilation жива, эти references трассируются сильно. Finalize формирует фиксированные vectors; `reallyAdd` переносит их в CommonData. Неактивный optimizing CodeBlock становится live только после доказательства независимой достижимости всех weak dependencies. Уже marked/active CodeBlock, наоборот, strongly visits weak references. [Pending roots](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGPlan.cpp#L656), [публикация](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/dfg/DFGDesiredWeakReferences.cpp#L71), [условная code edge](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/runtime/ScriptExecutable.cpp#L558), [liveness](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/CodeBlock.cpp#L1484), [active references](https://github.com/WebKit/WebKit/blob/e9c03b26665d2a3f4c9002075d90f9d9d5c1cb5e/Source/JavaScriptCore/bytecode/CodeBlock.cpp#L2028).

Для Otter это означает будущую единую фазу code liveness + entry retirement + active-frame roots при GC, а не превращение всех `LoadCellValue` в слабые ссылки. Остаточный TS шаг 41 МиБ пока не приписан этому владельцу: нужны observations конкретных generations/roots.

### Отвергнутый Array execution срез

Полная попытка заменить arbitrary-JS miss у Graph ArrayPush/Pop на no-JS probes, включая actual `CallWithThis`, обе ISA, COW copy-out и раздельные semantic/OOM exits, удалена из активного кода. Семантические/native/GC проверки проходили; artifacts показывали сохранение чужих field facts (4 loads → 1). Однако финальные три AB/BA-пары на восьми неизменённых классах не дали общего ускорения: DeltaBlue **+21,37% wall** (CPU 0,72 → 0,77 с), Box2D **+11,20%** (0,73 → 0,79 с), TS **+1,49%** (4,24 → 4,26 с). Это не принятый performance result. Все 64 процесса дали одинаковые результаты. Полные диапазоны/CPU/RSS, binary/source SHA, неудачные проверки и rejected patch: `benchmarks/results/array-cow-probes-2026-10-22/`.

Отдельно сохраняется найденное исправление корректности: shared tagged slab сообщает writable capacity=0, но хранит полный prefix. Прежний inline grow выделял `max(2*capacity,4)` и мог копировать восемь элементов в четыре слота. Обе ISA теперь резервируют `max(2*capacity,4,length+1)` до копирования. Этот fix не объявляется ускорением и не сохраняет отвергнутый no-JS refactor.

## Цель и критерий

Цель — конкурентная скорость полного исполнения, прогретого исполнения, startup и компиляции при минимальных peak RSS и удерживаемой памяти. Лидерство над другими движками должно подтверждаться на опубликованной матрице, а не одним TS-прогоном или снижением числа инструкций. Превосходство над всеми движками на любых программах не является проверяемым обещанием.

Работа идёт в текущем checkout. Все исходные изменения сохранены одним checkpoint-коммитом `fa4cc6668`. Последующий field-observer эксперимент не принят как достижение цели: сокращение emitted code около 17% дало только 2,7% медианного выигрыша полного ts, без убедительного уменьшения RSS. Его executable, patch и пять пар сохранены в `benchmarks/results/metadata-ownership-2026-10-21/compact-experiment/`. Observer-прототип полностью удалён из активного кода; его последний patch сохранён в `benchmarks/results/architecture-census-2026-10-22/rejected-observer.patch`.

## Законченный срез: наблюдения JIT перестают владеть JS graph

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

Проверенный затем ArrayPush/ArrayPop execution срез отвергнут по полному A/B; результаты и удалённый patch приведены выше. Текущий следующий execution срез — отмена ненужной фоновой компиляции с source lifetime до terminal ACK.


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


## Второй physical-field кандидат: семантика прошла, производительность не прошла

Кандидат `27ebff14f7ff42de0a1006ccf08b89b5ada818c66b844846998d7452d2a083b8` добавил неаллоцирующую миграцию старого экземпляра на уже подготовленный предпочтительный layout. ARM64 и x86_64 сохраняют live GP/FP и eager-only значения; после эффекта проверяют общий dependency flag поколения и точный физический shape. Отказ не имеет эффектов. Исправлен folded RegExp result builder: canonical birth values проходят тот же протокол подготовки до публикации.

Все 64 отдельных процесса восьми классов дали одинаковые результаты. Это **семантический pass, performance fail**. Три измеряемые чередующиеся пары и исключённая прогревочная пара на класс; production release без profile/env overrides. База — executable коммита `805ec838`, SHA `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a`.

| Класс | Wall до → после, с | Δ wall | CPU до → после, с | Peak RSS до → после, МиБ |
|---|---:|---:|---:|---:|
| crypto | 0.2039 → 0.2087 | +2.38% | 0.21 → 0.21 | 43.52 → 45.36 |
| deltablue | 0.6264 → 0.6706 | +7.06% | 0.65 → 0.71 | 54.70 → 55.95 |
| box2d | 0.6574 → 0.8387 | +27.58% | 0.76 → 0.92 | 135.58 → 138.47 |
| raytrace | 0.8549 → 1.1534 | +34.91% | 0.85 → 1.14 | 45.30 → 46.25 |
| splay | 0.5974 → 0.6139 | +2.76% | 1.01 → 1.01 | 370.80 → 374.28 |
| regexp | 1.2601 → 1.3577 | +7.75% | 1.27 → 1.37 | 140.67 → 139.83 |
| mandreel | 0.9948 → 1.0017 | +0.69% | 1.05 → 1.05 | 222.58 → 226.69 |
| ts | 2.9711 → 3.6179 | +21.77% | 3.96 → 4.57 | 492.70 → 495.25 |

Отдельный unchanged TS require + два принудительных GC: median live GC bytes 8 267 608 → 8 267 168; post-GC resident bytes 313 638 912 → 308 232 192 при перекрывающихся диапазонах. Это не существенное сокращение retained heap и не доказанный общий RSS выигрыш. Raw rows, process snapshots, ranges, stdout, executable и exact source patch сохранены вне очищаемого репозитория: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/shape-migration/`.

Свежие Box artifacts показали устранение части shape-exit poisoning, но compiled code всё ещё 2 001 944 против 1 440 456 байт у базы. Dependency checks выросли с 602 до 2235. Причина следующей правки установлена в compiler contract: representation guard ошибочно clobber-ил logical fields, field-type и dependency facts, хотя успешная миграция сохраняет все JS-значения и проверяет целый generation flag до продолжения. Следующая проверяемая ревизия инвалидирует физические MapWords, сохраняет decoded SSA/CSE и уже проверенные подписанные факты. Это пока гипотеза ускорения, а не принятый результат.


## Ревизия d402: правильные effects уменьшили регрессию, цель всё ещё не достигнута

Default-release executable SHA `d402eda27c6bf7de1b6d8faded460558a5916055db687f5b924c344736bf8a03`. Убраны ложные clobbers logical fields/FieldTypes/Dependencies у неаллоцирующей миграции; сохранена инвалидизация физических map proofs. Допустимые холодные writes и cached append сохраняют уже корректный старый physical prefix. Исправлена публикация shared store policy после committed megamorphic store; TOP owner уже поддерживался и причиной не был.

IR preservation, runtime migration/birth на обеих ISA, адресные VM regressions и 16 CLI сравнений с interpreter/GC 1,4,16 прошли. Все 64 A/B процесса совпали по результату; performance снова **fail**:

| Класс | Wall до → после, с | Δ wall | CPU до → после, с | Peak RSS до → после, МиБ |
|---|---:|---:|---:|---:|
| crypto | 0.2096 → 0.2355 | +12.35% | 0.21 → 0.23 | 43.83 → 43.36 |
| deltablue | 0.6331 → 0.6619 | +4.55% | 0.66 → 0.69 | 55.91 → 53.20 |
| box2d | 0.6164 → 0.6825 | +10.73% | 0.70 → 0.78 | 133.73 → 137.05 |
| raytrace | 0.8074 → 0.9843 | +21.91% | 0.81 → 0.99 | 46.66 → 46.06 |
| splay | 0.5953 → 0.6117 | +2.75% | 1.00 → 1.01 | 367.33 → 369.17 |
| regexp | 1.2695 → 1.3613 | +7.23% | 1.28 → 1.37 | 140.62 → 138.44 |
| mandreel | 0.9887 → 0.9880 | -0.08% | 1.04 → 1.04 | 220.89 → 229.62 |
| ts | 3.0220 → 3.6209 | +19.82% | 3.97 → 4.57 | 497.61 → 493.78 |

Raw команды, ranges, stdout, source/executable hashes и process snapshots: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/shape-effects/`. Это сравнение с принятой базой 805, а не с предыдущим замедлившим кандидатом.

Свежие RayTrace events полные (3656/3870), без uncached-store storm. В latest IR стало меньше nodes (3578 → 3513), но code вырос 292 696 → 349 216 B. StoreNamedProperty занимает 47 968 → 80 452 B; один constructor store вырос 28 → 172 B. Из 439 показанных оптимизирующих self samples нового launch-профиля 147 попали в StoreNamed, из них 58 — в повторный birth-target guard, 44 — в admission/normalization; 39 других samples — Float64ToTagged. Это частичная top-200 выборка, не доля всего времени и не отдельный performance benchmark. Profiler с artifact capture запускался отдельно от timed A/B.

Graph уже специализирует append programs и знает resulting shape. Следующий срез не добавляет второй transition engine: immutable parent/codec/slot facts проверяются при bake, текущая preference последнего поля подписывается через существующий generation dependency owner. Один selected program сохраняет Tagged RHS либо принимает F64 напрямую. Descriptor/prototype/extensibility/capacity guards, physical summary observation, source-stability notification и publication остаются у действующего native program. Промах выходит до первого эффекта на точный source StoreProperty; полное preferred состояние старого prefix не требуется.

Отдельное упрощение birth-протокола объединяет admission, preferred-vector check и encoding в один PreparedBirth вместо трёх проходов. Первый packet build отвергнут до performance-замера: source audit выявил mutable-parent reborrow при защищённом slab tail. Исправление разделяет shape/prefix/tail; callee один владеет tail roots, после возврата object tracing и initializer используют один mutable-derived pointer. Адресный фактический moving-GC overflow test прошёл на обеих ISA. Это исправление memory-safety контракта; прохождение output-тестов само по себе не использовано как доказательство отсутствия Rust alias UB.


### Safe birth packet: whole-run rejection and selected append closure

Default release `71ff1f772fa3a657ddec62857677cdea153ad61458ca68bad4dab25366172511` passed 23 CLI observations, including interpreter/Template/production comparison and affected GC strides 1/4/16. The unchanged eight-class alternating matrix passed all 64 output observations, including excluded warmups. Performance still fails against accepted `805ec838` executable `5651d0a7f1ba674de390cd3b3f1cd2981fcf87cbd1edb8894ccb4831465de69a`; this revision is not an accepted optimization.

| Workload | Wall median, s before → after | Wall delta | CPU median, s before → after | Peak RSS, MiB before → after | Instructions, B before → after |
|---|---:|---:|---:|---:|---:|
| crypto | 0.2340 → 0.2121 | -9.34% | 0.23 → 0.21 | 42.97 → 45.50 | 3.858 → 3.971 |
| deltablue | 0.6238 → 0.6608 | +5.92% | 0.65 → 0.69 | 54.78 → 54.58 | 11.960 → 12.339 |
| box2d | 0.6066 → 0.7787 | +28.37% | 0.69 → 0.87 | 133.91 → 135.78 | 9.574 → 10.672 |
| raytrace | 0.7884 → 0.9794 | +24.23% | 0.79 → 0.99 | 46.16 → 45.03 | 13.002 → 16.287 |
| splay | 0.6127 → 0.6446 | +5.22% | 1.01 → 1.07 | 371.23 → 368.09 | 8.948 → 9.013 |
| regexp | 1.2716 → 1.3245 | +4.16% | 1.28 → 1.33 | 140.91 → 138.42 | 18.538 → 19.271 |
| mandreel | 0.9889 → 0.9806 | -0.83% | 1.04 → 1.03 | 224.80 → 227.14 | 15.929 → 15.937 |
| ts | 2.9802 → 3.7196 | +24.81% | 3.96 → 4.71 | 497.78 → 492.97 | 33.746 → 37.154 |

Raw data, source/executable hashes, per-run ranges and host process snapshots: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/shape-birth-packet-safe/whole-ab/`. Box2D instructions stayed near the previous candidate while wall time varied substantially; this is evidence against attributing every wall delta to a single source change. RegExp work fell from the previous candidate, but remains above the accepted baseline. The all-thread TypeScript samples also show increased generated Graph and Template work, rather than a compiler-only problem.

The next coherent change keeps one existing CacheIR program as the owner of a selected append. Bake proves immutable parent/slot/codec properties; the existing generation dependency owns the last field preference. A numeric SSA operand writes its purified F64 word directly. A late post-phi refinement uses the same certificate when normal representation selection reveals a numeric phi; deopt frame states continue to reconstruct canonical Values. No speculative numeric assumption is introduced for arbitrary CallJs results. Source shape, descriptor, extensibility, capacity and admission checks still precede the first effect.

The focused runtime regression passed on native ARM64 and x86_64: actual normal-policy compiled IR contains the selected numeric append; Candidate → Double → General changes retire the right generation; a prototype setter and an argument callback each run exactly once; an FP value live across the constructor remains correct. The fixture obtains its numeric proof with ordinary `argument() * 1`. An earlier fixture without this operation correctly retained Tagged because the call result had no numeric proof; its failed strict IR assertion was diagnosed and the fixture corrected, without relaxing the IR requirement. Both ISA selected-append and dependency tests, plus the VM stale-own-codec / old-prefix certificate test, passed. Raw logs and the exact next compiled patch are in `shape-static-append/` outside the repository. Whole-run performance for this next candidate is still pending.


### Typed append result: physical shape formats rejected

Default release `43f7c80a4e2c0f58c1e5002fa91c13b0f9c4069a5b5a872f303fea53c4d66e47` passed all 23 CLI and 64 whole-run observations. The strict actual Runtime regression passed on ARM64 and x86_64. Against accepted 805 it still fails the performance/resource criterion:

| Workload | Wall delta | CPU median, s before → after | Peak RSS, MiB before → after | Instructions, B before → after |
|---|---:|---:|---:|---:|
| crypto | +0.07% | 0.21 → 0.21 | 43.73 → 44.16 | 3.857 → 3.964 |
| deltablue | +0.08% | 0.66 → 0.65 | 56.34 → 54.95 | 12.038 → 12.056 |
| box2d | +10.72% | 0.69 → 0.78 | 133.64 → 134.67 | 9.574 → 10.726 |
| raytrace | +3.65% | 0.80 → 0.84 | 46.59 → 46.61 | 13.005 → 13.727 |
| splay | +6.38% | 1.00 → 1.05 | 368.09 → 375.44 | 8.950 → 9.061 |
| regexp | +4.43% | 1.28 → 1.34 | 139.95 → 138.66 | 18.540 → 19.266 |
| mandreel | -2.05% | 1.06 → 1.04 | 218.16 → 226.17 | 15.925 → 15.938 |
| ts | +21.18% | 4.00 → 4.62 | 494.23 → 501.98 | 33.752 → 37.101 |

Raw commands/ranges/hashes/output/process snapshots: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/shape-static-append/whole-ab/`. Direct F64 append reduces the prior RayTrace regression substantially, but TS and Box2D still lose CPU and RSS does not compensate. Physical shape variants, preferences, representation dependencies and migration are therefore being removed as a model. No cache or epoch is being added to rescue them.

The replacement retains one heap-owned structural interner and stores either canonical Value words or purified Value-valid Double words in ordinary fields. Numeric writes preserve geometry. All Tagged reads canonicalize, including native object-global bindings; Int32 remains Int32 when naturally produced. Existing field-type words gain the free DOUBLE bit: raw encoded Double zero is DOUBLE, never an Int32 constant. Existing field-type dependencies permit Int32-only payload loads, Double-only F64 loads, or a mixed Int32/Double decode. Undefined, null, exact shape identities and TOP cannot earn a complete numeric bound. No new representation owner or metadata bank exists. Native FP stores retain source/eager recovery and the existing field summary/barrier/publication owners.

### Current primary-engine reference distance

A separate rotated three-engine matrix ran unchanged fixture text with one excluded warmup and three measured fresh processes per engine per fixture: 96 output observations passed. Otter is the accepted 805 executable, Node is locally installed `v24.16.0`, Bun reports `1.4.3` (its diagnostic identifies canary `1.4.3-canary.1+bf80d21c6`). Node and Bun use the identical `node:vm` classic Script runner because direct Bun module entry rejects the legacy fixtures' undeclared assignments. File reading and this runner are included in their CPU/wall/RSS; Otter uses its default production file entry. This is a whole-process workload comparison, not an isolated throughput or compiler-only measurement.

| Workload | CPU, s Otter / Node / Bun | Peak RSS, MiB Otter / Node / Bun | Wall, s Otter / Node / Bun |
|---|---:|---:|---:|
| crypto | 0.20 / 0.16 / 0.13 | 45.0 / 54.9 / 33.9 | 0.200 / 0.152 / 0.110 |
| deltablue | 0.66 / 0.32 / 0.51 | 54.2 / 71.8 / 53.6 | 0.633 / 0.289 / 0.406 |
| box2d | 0.68 / 0.51 / 0.55 | 133.5 / 99.6 / 72.0 | 0.600 / 0.302 / 0.287 |
| raytrace | 0.82 / 0.46 / 0.41 | 46.8 / 62.2 / 59.5 | 0.812 / 0.432 / 0.289 |
| splay | 1.04 / 0.50 / 0.50 | 381.6 / 390.4 / 220.6 | 0.614 / 0.300 / 0.184 |
| regexp | 1.30 / 0.45 / 0.41 | 140.7 / 76.2 / 60.0 | 1.300 / 0.408 / 0.341 |
| mandreel | 1.03 / 0.69 / 0.77 | 221.9 / 133.2 / 150.9 | 0.978 / 0.576 / 0.453 |
| ts | 4.05 / 2.31 / 2.72 | 491.2 / 504.6 / 467.9 | 3.054 / 1.176 / 0.990 |

Raw data and runner: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/reference-engines-805-script/`, `compare-engines.py`, `run-classic-script.cjs`. The first direct-module Bun attempt is preserved separately as failed and is excluded. These observations do not support a claim that Otter is already faster or smaller overall. A separate fresh RegExp launch profile contains 1357 all-thread samples: 636 VM runtime, 86 optimizing JS, 80 GC and 32 Template. Replacement/matcher/native protocol work remains a substantial independent target; it is not fixed by object shape changes alone. Profile raw data are in `numeric-field-words/regexp-profile-before/`.

### Geometry-only fields с канонизацией чтения: измеренный отказ

Default release `3ce990bdbf3db58c22cfff3edd5df1a20cbad223cfb9317f1cca9ca49f52e940` прошёл 106 адресных тестов на ARM64/x86_64, CLI differential/stress и 64 whole-run observations. Performance не проходит:

| Workload | Wall delta | CPU median, s before → after | Peak RSS, MiB before → after | Instructions, B before → after |
|---|---:|---:|---:|---:|
| crypto | -0.62% | 0.20 → 0.21 | 43.19 → 43.14 | 3.844 → 3.952 |
| deltablue | +10.40% | 0.65 → 0.72 | 54.23 → 52.47 | 12.021 → 12.829 |
| box2d | +4.83% | 0.71 → 0.74 | 134.45 → 132.05 | 9.573 → 9.842 |
| raytrace | +14.50% | 0.79 → 0.91 | 46.88 → 46.22 | 13.005 → 13.775 |
| splay | +2.31% | 1.03 → 1.04 | 375.45 → 370.88 | 8.961 → 9.127 |
| regexp | -1.26% | 1.27 → 1.25 | 139.39 → 140.38 | 18.538 → 18.573 |
| mandreel | -3.58% | 1.08 → 1.05 | 227.84 → 224.02 | 15.922 → 15.933 |
| ts | +15.35% | 3.97 → 4.38 | 497.23 → 500.09 | 33.744 → 35.041 |

Raw: `numeric-field-words/whole-ab/` вне репозитория. Отдельный exact-process RayTrace profile сопоставил все 799 Graph self samples с теми же runtime artifacts. 41 PC находится внутри read canonicalizers; 902 статические копии добавляют около 108 байт каждая. Это частичная доля whole-process времени, не обещание такого же ускорения. Более существенная регрессия: `rayTrace` имеет 8 NewReceiver против baseline 5, `blend` — 3 против 1, поскольку прежний escape pass отклонял Float64 stores/Number reads. Все конкретные потерянные sinks имеют нулевые observer owners; их можно восстановить, сохранив наблюдаемые записи. Новый срез устраняет обе причины через общий Value contract и typed virtual fields, без добавления cache.

### Подтверждённое удержание старых GC-страниц

Два последовательных неизменённых Splay запуска (805 и 3ce) с `--trace-gc` дали после full GC old extent/allocated около 262,5/62,6 и 259,5/61,7 МиБ. До GC extent/allocated почти совпадают, следовательно свободные диапазоны действительно возвращаются в allocator. Большая post-GC разница — фрагментация внутри частично живых 256-КиБ страниц nonmoving old space. Это не 198 МиБ лишних живых payload и не доказательство потери freelist. Сырой trace/time/hash: `numeric-field-words/splay-gc-diagnostic/` вне репозитория. Исследуется возврат полностью свободных OS pages внутри dead runs после finalizers; coverage и whole-run CPU/RSS ещё не доказаны. Blind GC growth-factor tuning и moving-old compaction в этот срез не добавлены.

### Universal Double Values: индексная граница найдена после whole-run отказа

Default release `8ac95d007f991ffcfcce07b901cb1840a9b32489d9372043f7d8fe84da92f866` прошёл 59 ARM64 и 60 x86_64 адресных cases, 25 CLI corpus observations, 7 RegExp observations и все 64 whole-run processes. Это correctness evidence, не performance acceptance:

| Workload | Wall delta | CPU median, s before → after | Peak RSS, MiB before → after | Instructions, B before → after |
|---|---:|---:|---:|---:|
| crypto | +337.87% | 0.21 → 0.90 | 43.36 → 43.97 | 3.851 → 14.501 |
| deltablue | -3.13% | 0.66 → 0.64 | 53.42 → 52.45 | 11.893 → 11.886 |
| box2d | +23.34% | 0.73 → 0.93 | 132.08 → 142.50 | 9.712 → 11.621 |
| raytrace | -11.63% | 0.83 → 0.74 | 46.12 → 46.48 | 13.002 → 12.586 |
| splay | -0.97% | 1.01 → 0.99 | 369.17 → 374.16 | 8.957 → 8.993 |
| regexp | -1.25% | 1.28 → 1.27 | 140.83 → 139.41 | 18.539 → 18.559 |
| mandreel | +8.52% | 1.03 → 1.15 | 220.47 → 226.31 | 15.925 → 17.165 |
| ts | -0.57% | 3.99 → 3.97 | 492.78 → 490.94 | 33.760 → 33.884 |

Raw `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/universal-number-values/whole-ab/` содержит hashes, commands, ranges и host process snapshots. Срез не принят. Exact-process crypto profile (`crypto-profile/`, `crypto-artifacts/`, `crypto-events.json`) показал 451/892 isolate inclusive samples в `jit_runtime_store_element_value`. В настоящем c25 `bnpSquareTo` цикл создаёт Float64Add → Float64ToTagged → StoreKeyedCached. Native cached index guard принимал только raw Int32; runtime dense completion тоже принимал только raw Int32. Поэтому каждый integral Double индекс превращался в slow property-key path. Это не повод вернуть канонизацию каждого property read: numeric indexed access получает собственную semantic projection в отдельный GP register, а оригинальный Value остаётся для named-table/runtime/typed-array/push-leaf miss. Fraction/NaN/Inf/negative/out-of-native-range inputs выходят до эффекта. `-0` адресует нулевой элемент. VM full-range array-index projection переиспользуется из одного существующего owner. Новые canary tests проверяют обе ISA и actual Template ARM address path; differential corpus проверяет fractional/NaN/max-u32 names и inherited setter once.

Отдельные exact-process Box2D artifacts baseline/new не доказывают время без diagnostic capture: I/O/assembly formatting в этих профилях существенно. Они используются только для локализации операций. До/после source/value изменений native Graph self samples составили 402/417; старые LoadOwnField+CheckedTaggedToFloat64 вместе114 samples заменены 105 LoadNumberOwnField samples. Число typeMismatch recompile exits выросло 3 → 14. Следующий unchanged A/B определит, устраняет ли индексная граница whole-run регрессии.

### Числовое значение и физическая кодировка: окончательная граница

Index-only `0d81a3540559e64c65130d4d9217643edcb363a567690f28845722ef75195355` прошёл 124 адресных cases, 32 CLI и 64 whole-run observations, но CPU оставался хуже baseline: crypto0,20→0,37, Box2D0,70→0,90, Mandreel1,04→1,14, TS3,91→4,09. Raw: `numeric-indices/whole-ab/` вне репозитория. Он не принят.

Semantic release `9b275330b9b2fab728cc845d2c879180209222822481eaae48509cede9dd26fd` разделяет numeric Int32 speculation и physical Int32 word proof. Единственный Rust classifier `NumberValue::as_exact_i32` использует точный roundtrip и исключает `-0`; его используют arithmetic feedback, parameter widening и explicit canonicalizing constructors. `CheckedNumberToInt32` обеих ISA принимает оба Number encoding и сохраняет исходный boxed input для eager recovery, включая совпавший destination register. Raw `as_i32`, FieldType, field-load forwarding и virtual raw-I32 reads остаются физическими. `Known.int32` даёт Tagged operand лишь Number union, никогда raw INT32 bound. ARM Template использует тот же смысл numeric class; x86 Template собственного inline feedback writer не имеет. Новая actual Runtime-регрессия проверяет Double bits настоящего producer, установленный Int32Add IR, отсутствие повторных deopt/Template entries и единственный выход на `-0`.

135 адресных cases (ARM68/x8667), 32 CLI и 64 whole-run processes прошли. Итог ещё не принят как общий performance выигрыш:

| Workload | Wall delta | CPU median, s before → after | Peak RSS, MiB before → after | Instructions, B before → after |
|---|---:|---:|---:|---:|
| crypto | +0.46% | 0.21 → 0.21 | 43.41 → 45.34 | 3.851 → 3.966 |
| deltablue | +2.65% | 0.65 → 0.67 | 53.91 → 54.08 | 11.884 → 12.059 |
| box2d | +5.96% | 0.69 → 0.74 | 132.34 → 133.09 | 9.576 → 9.450 |
| raytrace | -9.46% | 0.80 → 0.72 | 46.33 → 46.19 | 13.002 → 12.576 |
| splay | -1.81% | 1.00 → 1.00 | 372.92 → 371.62 | 8.943 → 8.943 |
| regexp | +3.53% | 1.29 → 1.33 | 140.84 → 140.59 | 18.540 → 18.573 |
| mandreel | +3.05% | 1.04 → 1.06 | 220.27 → 224.44 | 15.927 → 16.204 |
| ts | +2.01% | 3.93 → 4.02 | 486.67 → 500.11 | 33.743 → 34.005 |

Raw: `semantic-integers/whole-ab/` вне репозитория. Следующая адресная проверка устраняет повторную semantic FP classification после насыщения существующего FLOAT bit. Это тот же монотонный feedback owner, без нового cache или epoch.

### GC startup и allocator: проверенные границы следующего среза

Pinned V8 использует те же bounds1,1…4 и target mutator utilization0,97 (`src/heap/heap-controller.{h,cc}`), поэтому простое уменьшение коэффициента не является переносом архитектуры. Его default preconfigured old space32MiB; legacy initial-limit shrink применяется только без заранее заданного размера. JSC `e9c03b26665d…` имеет Small heap1MiB и Large/shell до32MiB; VM embedding defaultSmall, Bun choice данным checkout не доказан. На ARM и RAM≥16GiB proportional factor близок4: `1+3*exp(-2*min(H,RAM)/RAM)`, где H включает visited cells и external bytes (`Heap.cpp:141–195,2583–2656`). Поэтому Bun RSS нельзя объяснить универсальным1,5×live.

Otter пока сохраняет initial old limit256MiB до первого full, несмотря на реальные minor survival/promotion counters. Готовится один first-successful-minor seed существующего контроллера: conservative working set = old consumed after minor + copied survivors; promoted и promotedLarge уже входят в old и не суммируются повторно. Используются существующие factor/headroom/floor/cage bounds и текущая nursery capacity. Seed однократный; explicit initial limit сохраняет владельца, последующие major используют прежнюю динамическую формулу. Это будущий кандидат, ещё не измеренный.

Неизменённый TS на `0d81` впервые собирает old234,2MiB→33,2MiB extent/9,8MiB allocated; ранний стартовый feedback имеет реальный target для peak RSS. Splay после full по-прежнему имеет около261/62MiB extent/allocated. Raw: `numeric-indices/gc-diagnostic/`.

Три чередующиеся TS retention пары после warmup дали baseline805 postGC RSS295,95MiB и live7,88MiB; `0d81`293,17MiB и7,87MiB. Forced-full old extent после этого31,5/33MiB; весь RSS нельзя приписать old holes. Отдельный causal allocator experiment использовал один immutable `9b2753` binary: default mimalloc purge delay1000ms против `MIMALLOC_PURGE_DELAY=0`, без сборок/профилей. PostGC RSS296,81→298,34MiB, CPU4,16→4,26s; peak RSS494,72→491,30MiB находится в перекрывающихся диапазонах. Настройка не принимается. Raw: `semantic-integers/allocator-diagnostic/`. Mimalloc3.3.2 essential stats доступны, но exit CURRENT печатается после force collect и не выдаётся за внутренний postGC snapshot.


### 2026-10-23 01:48 UTC — numeric Value model frozen, physical RSS attribution

Default-release numeric candidate SHA-256 `60db42190c136b1a80566542084b69bf95ffdc6ff3781446a1fe8e6de19b4f28` passed the focused ARM64/x86-64 gates, 32 CLI differential/stress observations and all 64 observations on the unchanged eight-class workload. Semantic success is separate from performance acceptance. Three alternating measured pairs plus one excluded warm pair per class:

| Workload | CPU before → after, s | Peak RSS before → after, MiB | Wall delta |
|---|---:|---:|---:|
| crypto | 0.21 → 0.21 | 44.94 → 44.88 | +2.43% |
| deltablue | 0.66 → 0.66 | 54.28 → 55.28 | -0.24% |
| box2d | 0.68 → 0.73 | 130.64 → 132.70 | +3.58% |
| raytrace | 0.81 → 0.74 | 46.38 → 45.78 | -9.45% |
| splay | 1.02 → 0.99 | 369.83 → 368.33 | -2.95% |
| regexp | 1.28 → 1.28 | 140.77 → 140.25 | -2.06% |
| mandreel | 1.04 → 1.07 | 220.62 → 223.89 | +1.68% |
| ts | 3.97 → 3.96 | 495.12 → 492.00 | +0.76% |

Raytrace improves, Box2D CPU regresses despite fewer retired instructions, and the other deltas are mixed. This candidate is not an overall engine speed or memory breakthrough. Raw: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/feedback-saturation/whole-ab/results.json`.

The default-binary vmmap diagnostic ran after unchanged TS require plus two explicit full collections, then held the process with a timer only to inspect mappings. Approximately 96–100 MiB resident belongs to the inferred GC cage, 25.25 MiB to 988 retained code RX mappings, and about 152 MiB lies in mimalloc-tagged mappings demonstrably outside the cage. The `IOAccelerator` label is mimalloc OS tag 100, not GPU allocation. Mapping attribution cannot distinguish live Rust owners from allocator holes; immediate mimalloc purge delay already failed to lower RSS in the same-binary causal diagnostic. Do not attribute the entire retained footprint to GC old-space fragmentation. Raw topology, commands and hashes: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/feedback-saturation/vmmap-diagnostic/`.

Next bounded resource slices: seed the initial old-generation budget from the first successful nursery survival, then return wholly unused nursery OS pages only after full collection. Keep old-space growth coefficients unchanged; verify full CPU/peak RSS A/B plus distinct post-full-GC retention before claiming a win.

### 2026-10-23 02:07 UTC — nursery physical discard, measured resource/CPU tradeoff

Default release SHA-256 `3b5f20c54b2448524669af778c857c6de75842780fab482629524028ba48ccb7` contains full/split-sweep-only inward OS-page remapping of inactive nursery payload and active unused tails. The same existing cage remap owner also serves freed whole-page recycling; unsupported/failed unused-range remaps do not zero or touch memory. Headers, live prefixes, virtual addresses, capacity and ordinary minor reuse are retained. Full pause accounting includes discard time. Independent lifecycle review, 2 actual OS/live/capacity/reuse tests, 3 startup tests, all 8 collection observation cases, cap admission, and 32 CLI differential/stress runs passed.

| Workload | CPU before → after, s | Peak RSS before → after, MiB |
|---|---:|---:|
| crypto | 0.21 → 0.20 | 43.19 → 43.53 |
| deltablue | 0.66 → 0.65 | 55.19 → 53.98 |
| box2d | 0.70 → 0.71 | 132.61 → 119.45 |
| raytrace | 0.78 → 0.72 | 46.27 → 45.67 |
| splay | 1.01 → 1.09 | 369.92 → 335.12 |
| regexp | 1.28 → 1.31 | 140.91 → 99.75 |
| mandreel | 1.03 → 1.07 | 223.09 → 223.33 |
| ts | 3.91 → 4.11 | 502.83 → 410.89 |

64/64 unchanged-workload observations match. Separate TS require + two explicit full GCs: RSS median 311689216 → 241336320 B, live JS 8267640 → 8255704 B. Strong footprint improvement has a CPU price, particularly Splay (+8%) and TS (+5%); it is not accepted as a general speedup. Incremental A/B against startup-only binary and existing V8/JSC footprint-versus-throughput GC policy are being checked before selecting the final policy. Raw: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/nursery-discard/`.

Actual-argument slices omitted from deopt/safepoint `retained_bytes` now count toward generated-code admission. Two extended existing tests passed. This accounting correctness fix is not advertised as RSS savings; installed Graph/AST leakage was not proven. Exact ownership audit: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/deopt-arguments-accounting/ownership-audit.md`.

### 2026-10-23 02:45 UTC — финальный Otter против актуальных локальных Node/V8 и Bun/JSC

96 свежих процессов: три измеренных наблюдения и warm observation на каждый engine/class, с вращением порядка. Все stdout совпадают. Node v24.16.0, Bun 1.4.3. Одинаковый `node:vm Script` wrapper для Node/Bun сохраняет classic-script semantics неизменённых старых fixtures; чтение wrapper/source и startup включены, Otter использует обычный file entry. Это whole-launch сравнение, не чистый mutator throughput.

| Workload | CPU Otter / Node / Bun, с | Peak RSS Otter / Node / Bun, MiB |
|---|---:|---:|
| crypto | 0.21 / 0.17 / 0.13 | 44.94 / 54.81 / 34.06 |
| deltablue | 0.65 / 0.33 / 0.52 | 53.42 / 71.48 / 53.39 |
| box2d | 0.72 / 0.51 / 0.57 | 120.08 / 99.83 / 71.44 |
| raytrace | 0.72 / 0.46 / 0.41 | 46.05 / 62.31 / 60.41 |
| splay | 1.07 / 0.52 / 0.51 | 374.16 / 384.61 / 216.25 |
| regexp | 1.29 / 0.45 / 0.42 | 102.88 / 76.30 / 59.94 |
| mandreel | 1.06 / 0.69 / 0.80 | 223.22 / 133.38 / 151.31 |
| ts | 4.08 / 2.24 / 2.73 | 382.17 / 503.39 / 390.61 |

CPU Otter остаётся хуже обоих reference engines на всех восьми классах этого захвата. Снижение памяти относительно собственного baseline не означает общего лидерства. RegExp/Splay — явные CPU targets; Box2D/Mandreel также сохраняют большой RSS gap с Bun. Raw, versions, executable/source hashes, process snapshots и wrapper note: `/Users/alexanderstreltsov/.codex/artifacts/otter-2026-10-22/reference-engines-final/results.json`.
