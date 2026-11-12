# Архитектура и сборка Otter

## Исходное состояние

2026-11-12, HEAD `9c47049b721162ce850bf2427becec27d4797056`.
До работы изменён `vendor/test262`, не отслеживаются `.claude/` и `tests/test262/`;
они не затронуты. Коммитов нет. AGENTS.md и ES_CONFORMANCE.md прочитаны;
исторические 99.98% не использованы как доказательство текущей корректности.
`fff` недоступен, поиск выполнен через `rg`.

macOS Darwin 25.6 ARM64, 12 CPU, 36 GiB RAM, в начале 85 GiB свободного диска.
Load average 9.28 / 6.00 / 5.31: машина занята. Установлен отсутствовавший
закреплённый `beta-2026-10-18`: rustc/Cargo 1.100.0-beta.7, LLVM 23.1.4.
Toolchain-файл не менялся. Downloads выполнены до измерений; offline-проба
на установленном 1.99.1 без jsonc-parser не считается baseline.

## Workspace по manifests, metadata и исходникам

Стрелка означает «зависит от». Перечислены основные публичные поверхности.

| Crate | Назначение и граница |
|---|---|
| resource | ResourceAccount, лимиты, leases, snapshots |
| gc | Heap, GcSession, HandleScope, Local → resource |
| bytecode | BytecodeModule, verifier, opcode_schema, binary/disasm/dump → resource |
| syntax | SourceKind, with_program, SyntaxError → OXC |
| compiler | compile, CompiledModule, CompileOptions → syntax, bytecode, regex validation |
| regex | validate_utf16, Flags; runtime feature: Regex/MatchScratch/native tier → ICU |
| vm | Interpreter, Value, NativeCtx, builders, JIT contracts → gc, bytecode, resource, regex, macros |
| jit | Native compiler/artifacts → vm, bytecode; GC только dev dependency |
| runtime | Runtime/RuntimeBuilder, Otter, RuntimeHandle → vm, jit, compiler, syntax, gc, bytecode, resource, xml, http-client |
| http-client | Client/RequestBuilder/Response → hyper, rustls |
| xml | parse_bytes/parse_utf8, Node, sink; без dependencies |
| macros | HostClass/js_class и binding generators → syn/quote |
| modules | OtterModulesBuilderExt, serve/sql/kv/ffi/xml → runtime, macros, xml |
| node | Hosted Node modules/globals → runtime, vm; compiler в build script |
| web | WebApiBuilderExt, wasm/crypto/fetch → runtime, macros; compiler в build script |
| pm-manifest | PackageManifest, workspace discovery/settings → serde, Tokio |
| pm-lockfile | Lockfile, LockedPackage, import formats → serde JSON/YAML/TOML |
| pm-phantom | scan_project/extract_specifiers → manifest, syntax, OXC |
| pm | Installer, registry, resolution, policy → manifest, lockfile, http-client |
| cli | otter/otterx → runtime, modules, node, web, PM, syntax, bytecode |
| vm-codegen | Отдельный gen-pow10, не dependency VM |
| benchmark | Schema/recorder; engine, rss, warm-harness включаются features |
| difftest | Process runner без engine dependency |
| test262 | Runner/report/isolation → runtime, vm |
| node-compat | run(RunOptions), RunReport, CLI без engine dependency |
| extension-template | Пример → runtime, macros |

Префикс всех имён — `otter-`. Runtime соединяет frontend и VM: VM не зависит
от compiler, GC не знает VM, JIT потребляет VM-owned контракты. Runtime → Node/Web
и macros → VM/GC встречаются только в dev dependencies. Bytecode нужен и runtime
consumers, и host producers Node/Web; поэтому его правка запускает широкий rebuild.
CLI имеет десять внутренних normal/build зависимостей, runtime — девять.
Runtime экспортирует advanced Value/NativeCtx API, но утечка через Send RuntimeHandle
не доказана. Generic monomorphization как причина стоимости также не доказана.

### Почему тяжёлые зависимости остаются

- CLI → web → Wasmtime/Cranelift: `wasm.rs`, `wasm/asm/instance.rs` действительно
  компилируют модули. `wat` влияет на принимаемые входные байты, не только tooling.
- CLI → modules → rusqlite/bundled: SQL/KV; runtime/PM → http-client → rustls →
  aws-lc: TLS для fetch/modules/registry. Удалять без потери функций нельзя.
- VM → ICU compiled_data/Temporal; compiler → regex → ICU properties:
  Intl и Unicode RegExp, не неиспользуемые таблицы.
- JIT → yaxpeax: `artifact/assembly*.rs`; диагностика выключена при исполнении,
  но входит в стандартную CLI и компилируется всегда.
- Bytecode serde/serde_json используются контейнерами и `dump::to_json_pretty`;
  runtime JSON5/JSONC/YAML/TOML — `data_modules.rs`.
- Criterion — dev dependency. Node `primordials-codegen` и benchmark engine —
  optional. Tokio features широки, но нужны полной CLI. OXC/syn/clap/serde/dynasm
  proc macros реально используются. Host/target compiler units собираются отдельно.
- Дубли syn 2/3, hashbrown 0.14–0.17, num-bigint 0.4/0.5 и wasm tooling имеют
  разных consumers. CLI включает wasm-encoder 0.251 через Wasmtime и 0.253 через
  asm/WAT. Metadata с dev/target packages не равен реально собранным units.

## Что изменено

1. Удалены прямые Node → flate2 и phantom → oxc_span: обращений нет,
   zlib использует libz_rs_sys. Обе библиотеки остаются транзитивно; число packages
   и features после этих удалений не изменилось (525 packages).
   Node normal dependencies: 18 → 17, phantom: 10 → 9, с target-specific entries.
   На первом проходе Web → GC сохранялась из-за generated `::otter_gc`;
   это ребро устранено в следующем шаге ниже.
2. По дополнительному запросу обновлено семейство OXC 0.154 → 0.158, без обновления
   resolver 11.23/toolchain. Consumers переведены на JSStr; старый decoder
   `U+FFFD + hex` удалён. Host specifiers проверяются, не преобразуются с потерями.
   По явному разрешению `TemplateSite.cooked` хранит UTF-16; обновлены binary IO,
   serde shape, accounting, compiler и VM. Старые артефакты несовместимы.
3. После запроса на сокращение всего workspace убраны дубли в существующем ядре:
   `dispatch_ops.rs`: 110 одинаковых `top_frame!` → одно определение;
   `method_ops/mod.rs`: два одинаковых root carriers → FastDispatchRoots;
   `native_abi/runtime_stubs.rs`: 139 дескрипторов, symbol lookup и dense inventory
   генерируются из одной таблицы. Все восемь полей, имена, ids и порядок сохранены.
   У `array::push` обнаружено stale value при GC во время роста slab: теперь
   используется корректный `push_with_roots`. Две реализации indexed store также
   сведены к одной. Существующий remembered-array тест воспроизводит ошибку;
   он теперь сам включает stress, а не зависит от окружения.
   Нет новых allocations/dynamic dispatch в hit paths, изменений Value layout, barriers,
   safepoints или deopt ABI.

Тесты сокращены по смыслу, не ради зелёного результата: убраны повторные helper
checks OXC и дубли property-key матрицы, уже покрытой `property_key_code_units`.
У primordials удалены магические количества 277/192: fixture-проверка состава,
хешей и проверка idempotent regeneration остаются. Terminal restore проверяется
без непереносимого открытия `/dev/fd`; порядок и однократность сохранены.
`eval_value` использует ES Promise job, а не отсутствующий в bare Runtime Web global.
В `jit_constructor_gc` убрана проверка фиксированных первых байтов пролога:
sloppy receiver guard законно находится перед ним. Проверки точного code object,
call-entry offset, native execution, safepoint/return-site и relocation сохранены.
Math-проверки учитывают существующий `CheckNative` eager exit; replacement/getter
по-прежнему обязаны исполниться ровно один раз. Удалена дублирующая numeric-leaf
матрица с неверным обещанием отсутствия повторных Graph deopts: журнал показывает
сначала ShapeGuard, затем TypeMismatch при новом receiver/callee. Общая differential
матрица, leaf-hit counters, порядок вычислений и moving-GC проверки остаются;
уникальные Set miss и rope search перенесены в общую матрицу.
Shape-directory тест теперь проверяет два effectful `LoadPropertyCached`, а не
устаревший commoned `LoadShapedProperty`: accessor fallback не должен replay или
склеивать обращения. Код lowering для этих исправлений не менялся.
Исправлены lint blockers без подавления lint; большая test-only CompileInput payload
упакована в Box, production representation не меняется. Исправлен излишний отказ
OXC helper на неиспользуемом ключе атрибута: сравнивается JSStr, не UTF-8 имя.

Объём считается по одним и тем же Rust-файлам в `crates/` и `examples/`, включая
комментарии, тесты и два новых файла OXC, без vendor и parked crates. Перед
расширенным проходом — 728 130 строк; после — 725 944: **−2 186 строк**.
В `dispatch_ops.rs` убрано 726 строк, в таблице stubs — 1 202.
Это первый проход, **не сокращение всего репозитория вдвое**. Перенос строк в макрос
сам по себе не ускоряет сборку; выигрыш здесь — один владелец повторяемых контрактов.

## Измерения

Артефакты: `/tmp/otter-architecture-review/`: metadata, trees, `measurements.jsonl`,
журналы, parser-probe. Старые timings/CLI binaries сохранены в `saved-artifacts`;
три собственных build-кэша удалены (около 38 GiB). Финальный `target-migration`
оставлен. Пользовательский target/caches не удалялись.

Измерения CLI: одна `git archive HEAD` копия с патчем after, разные пустые target
каталоги, profile dev, default features, jobs по умолчанию, заполненный registry
cache, page cache не сбрасывался. Stable Cargo timings, без self-profile.

```sh
cargo metadata --locked --format-version 1
cargo tree --offline --locked -p otter-cli -e normal,build
cargo tree --offline --locked -p otter-cli -e features
cargo tree --offline --locked -p otter-cli -d
cargo build --offline --locked -p otter-cli --timings --target-dir "$TARGET"
cargo check --offline --locked -p otter-cli --timings --target-dir "$TARGET"
```

| Наблюдение, с | HEAD | Удаление 2 рёбер | OXC + UTF-16 |
|---|---:|---:|---:|
| Clean build, 1 запуск | 115.38 | 109.51 | 134.19 |
| Первый check после build, 1 запуск | 66.64 | 75.82 | 90.53 |
| No-change check | 0.43 | 0.62 | 0.68 |
| VM edit, первый sample | 8.97 | 9.43 | 9.80 |
| Bytecode edit, 2 samples | 9.71 / 9.98 | 12.62 / 9.42 | 13.09 / 9.25 |
| VM edit после восстановления bytecode | 8.96 | 8.31 | 9.42 |

Trigger — комментарий в VM/bytecode lib.rs только в копии. Второй VM sample
включает восстановленный bytecode: это не чистый VM-only rebuild. ABI-типы не
менялись для измерения. Полная матрица ограничена стоимостью нескольких build trees.
**Ускорение Rust build/check не подтверждено.** Один запуск на занятой машине
не доказывает ни улучшение, ни устойчивую регрессию. Таблица относится к этапу OXC,
не измеряет последующее удаление дублей в ядре.

Baseline timings: ICU experimental 49.10 с (frontend 48.20/codegen 0.90),
VM 24.12 (14.76/9.36), compiler units 12.88 (3.65/9.23) и 13.97 (4.89/9.08),
runtime 11.30 (4.97/6.33). aws-lc-sys build script 42.34 с, zstd-sys 13.52,
Cranelift 21.67. CLI unit 6.95 с — не linker-only measurement. Параллельные units
не суммируются. After также замедлились неизменённые ICU (58.63) и aws-lc (54.05).

Parser-probe: обе версии в одном release binary, 225 Node JS-файлов / 2 842 685
bytes, SourceType::cjs + allow_return_outside_function, 20 проходов/sample,
6 чередующихся пар, первая исключена. Diagnostics = 0. Медианы 0.240065 →
0.190775 с (**−20.5%**), диапазоны 0.237034–0.245666 / 0.188850–0.196062.
Это parser-only результат, не ускорение всего Otter. Команда в `parser-probe`:
`RUSTUP_TOOLCHAIN=beta-2026-10-18 cargo build --release --offline`, затем binary.
[OXC release](https://github.com/oxc-project/oxc/releases/tag/crates_v0.158.0).

Материализация 2000 ASCII tagged sites, dev interpreter, performance.now без
startup/parser, 6 чередующихся пар минус прогрев: 109.97 → 109.83 ms;
значимого runtime-изменения не видно. UTF-16 payload дороже ASCII по памяти:
accounting считает capacity × 2. Подробности контракта —
[frontend.md](../site/src/content/docs/engine/frontend.md).

Узкий probe после сокращения ядра: 20 000 object pushes, indexed overwrites и
reads, пять проходов после прогрева; before — сохранённая OXC 0.158 CLI до этого
прохода, after — текущая CLI. Оба dev/default, 6 чередующихся пар на tier, первая
исключена, startup/parser вне таймера; checksum одинаков. Медианы, ms:

| Tier | Before | After |
|---|---:|---:|
| Interpreter | 1150.29 | 1149.67 |
| Template | 497.18 | 500.96 |
| Production tier policy | 121.95 | 122.98 |

Команда: `python3 /tmp/otter-architecture-review/followup/array-probe.py`.
Это smoke benchmark debug binary на занятом host, не доказательство отсутствия
регрессий в release и не заявка на ускорение. Оптимизированный benchmark ещё нужен.

## Проверки

Команды используют отдельный `CARGO_TARGET_DIR`, pinned toolchain, offline/locked.
До сокращения ядра: default CLI build, workspace all-target/all-feature check,
compiler 145, syntax 13, bytecode 113, phantom 20, Web 11 + 51 integration,
zlib 2, warm-harness 6 passed. Test262: одинаковые path → outcome до/после на
локальном corpus: template-literal 59, string 73, module-code 595 passed / 4 skipped.
Это 727 passed, не полный Test262. Новый UTF-16 runtime test прошёл strides 1–16.

После сокращения ядра: `cargo test -p otter-vm -p otter-jit -p otter-runtime
-p otter-node --lib --all-features --no-fail-fast`: VM 1305, JIT 349,
runtime 277 + 1 ignored, Node 29 passed. Для IPC/FSEvents тесты запущены с доступом
к локальным системным API: прежние sandbox failures не были production bugs.
После повторной сборки frontend: compiler 145, syntax 11, bytecode 113 passed.
Remembered-array: четыре теста проходят на каждом stride 1–16. Strict workspace
Clippy и fmt проходят без подавлений. Default CLI собрана; linker предупреждает
о большом `__eh_frame` в dev binary, proc-macro-error2 — о future incompatibility.
`bash scripts/gate.sh --quick all` проходит при `OTTER_GC_STRESS=1`: GC,
constructors, Math, native leaves и property tests. Дополнительные runtime tests:
eval 5, OXC 2, property-key 1, class-value 1, XML 6 passed; OXC — также на strides
1–16. Полные журналы этого прохода находятся в `followup/`.

Старые native-leaf и shape-directory failures воспроизведены отдельно на HEAD/OXC
0.154 в `target-baseline-followup`, без общих workspace artifacts с текущей сборкой.
Math/constructor failures также воспроизведены до исправления fixtures.
Это отделяет исправления старых тестов от GC-багфикса и OXC-миграции.

Ранее fmt и strict Clippy падали на HEAD; старые предупреждения теперь исправлены,
а не подавлены. Предыдущий differential был прерван после трёх минут; полный
диагностический запуск после OXC-миграции прошёл за 743.75 с: это не был deadlock.
Полный differential после GC-исправления: 198 программ, interpreter vs Template,
включая completion, console и ошибки — passed (`integration-final.log`).
Полный checked/release GC corpus и другие ОС/архитектуры пока не проверены.

## Риски и границы результата

- UTF-16 меняет один текущий bytecode/serde format без compatibility layer;
  старые сохранённые модули нужно пересоздать. ASCII cooked payload стал больше.
- Главный риск таблицы stubs — неверный descriptor. Сравнены все 139 строк,
  восемь полей, имена и порядок; ABI и validation tests сохранены.
- Общий путь array stores меняет rooting при allocation. Проверены regression,
  strides 1–16 и три runtime tiers; полный optimized GC gate остаётся необходимым.
- −50% LOC не достигнуто. Не удалялись пользовательские возможности, vendor,
  GC/JIT safety tests или parked reference code ради этого числа.

## Следующий выполненный шаг: frontend без RegExp matcher

`compiler/expr/literal.rs` использует `validate_utf16`, вызывающий тот же
`parser::parse`, что и `Regex::compile_utf16`. Убраны IR lowering и создание
NativeTier из literal validation; один UTF-16 buffer используется и для проверки,
и для bytecode constant. Второго parser, нового crate и публичного AST нет.

`otter-regex` имеет default-on `runtime` feature. Workspace compiler сохраняет
`unicode`, но не matcher; VM явно включает `runtime`. Native matching и стандартная
CLI не урезаны. ICU casemap, memchr, smallvec и assembler dependencies относятся
к runtime; Unicode property tables остаются у parser. В compiler-only normal/build
графе **97 → 86 packages**: ушли dynasm/dynasmrt, их вспомогательные зависимости,
ICU casemap/data, libc/memmap2. Это не удаление этих библиотек из полной CLI.

Parser-only использует тот же внутренний AST. Только его payload-типы имеют
обоснованный conditional `dead_code` allow: validation не читает matcher operands;
runtime по-прежнему проверяется без этого allow. Методы lowering и ASCII bitmap
вообще не компилируются без `runtime`. Заодно исправлен существовавший build blocker
`--no-default-features`: `v` parser обращался к отключённому `string_props`.
Unicode-off variant теперь отвергает property escapes, как требует его прежний
контракт; обычный compiler всегда включает Unicode и не теряет грамматику.

Измерения `/tmp/otter-regex-boundary/measure.py`: отдельные before/after copies,
три чередующиеся пары пустых targets, pinned beta, dev, offline/locked,
`cargo check -p otter-compiler --timings`; registry/page cache тёплый.

| Pair | Before, с | After, с |
|---|---:|---:|
| 0 | 58.18 | 24.10 |
| 1 | 36.08 | 21.08 |
| 2 | 15.42 | 14.70 |

Первые пары пересекались с другими сборками. Большой разброс не позволяет честно
объявить процент ускорения. Доказаны меньший dependency graph и отсутствие matcher
construction при validation; эти значения не являются full CLI clean-build timing.
Снимок after для измерений сделан до исправления Unicode-off конфигурации;
измеренная Unicode-on compiler конфигурация функционально та же.

В full `scripts/gate.sh` добавлена отдельная проверка compiler/parser-only:
workspace all-features не может проверить эту границу из-за feature unification.
Default matcher tests прошли на ARM64 и как исполняемые x86_64 macOS binaries
(47 unit, 49 matching, 1 validation); x86 — не только cross-check. Compiler —
145 tests, parser-only Unicode — 13 unit + 1 validation. Minimal без Unicode
также проверен отдельно. Strict workspace Clippy, compiler-only Clippy и
parser-only Clippy проходят; default CLI и Node/Web build scripts собираются.

Test262 `language/literals/regexp`, 4 workers, прежний timeout 10 s: **242 pass,
4 timeout до и после**, совпадают все 246 path → status. Четыре timeout —
`S7.8.5_A1.1_T2`, `A1.4_T2`, `A2.1_T2`, `A2.4_T2`: длинные переборы code units
с eval. Они не объявлены passed; миллисекунды срабатывания timeout не сравниваются.
Before runner собран из изолированного снимка непосредственно перед этим шагом,
не из старого OXC 0.154. Reports: `test262-before.json`, `test262-final.json`,
`test262-comparison.json` в `/tmp/otter-regex-boundary`.

## Следующий шаг: зависимости декларативных расширений

Без прямого `otter-gc` пример расширения не собирался: `couch!` и `holt!`
выдавали E0433 из generated code. Это воспроизведено до исправления после
удаления только dependency из manifest (`missing-dependency-before.log`).

`holt!`, `couch!` и inheritance resolvers `js_class` теперь получают `GcHeap` и
`RootScope` через существующий `otter_vm::__macro_support`; runtime уже отдаёт
этот контракт binding crates. Добавлены ровно два re-export, не весь GC module.
Это те же типы того же владельца: нет wrapper, conversion, нового trait или
runtime dispatch. Root slots, их время жизни, порядок аллокаций и barriers не
менялись. `__macro_support` остаётся скрытым контрактом generated code, не API
для handwritten extensions. Явные GC-derives `Pelt`/`Groom` сохраняют свою
обоснованную зависимость от GC.

Удалены прямые зависимости `otter-web → otter-gc` и `extension-template → otter-gc`;
normal dependencies второго crate — только runtime/macros; VM/GC нет даже в dev deps.
Шаблон собирается с `#![forbid(unsafe_code)]`: handwritten binding code не должен
использовать unsafe; внутренние root operations остаются ответственностью engine.
Существующие тесты template проверяют class, namespace, async и hosted module;
в full gate добавлен их запуск. Web `File extends Blob` проверяет native inheritance
через ту же границу. Обновлены оба руководства для bindings/embedding.

Это устранение скрытого требования к consumer manifest, не ускорение сборки:
GC остаётся транзитивной зависимостью runtime. Новых tests/crates нет; регрессия
проверяется существующим реальным consumer. Журналы: `/tmp/otter-macro-boundary/`.

Проверки этого шага: macros 14, extension-template 2, Web 72 passed / 1 ignored;
GC stress 1–16 — по 10 существующих проверок на stride (160 passed): template,
couch, holt и три Web-сценария, включая native inheritance и async Blob reads.
Fetch-тестам понадобились loopback sockets: sandbox EPERM отделён от результатов
движка, повтор с разрешёнными локальными sockets прошёл. Default CLI, strict
workspace Clippy, formatting и shell syntax gate проверены. Полный closing gate
не запускался; bytecode/Value layout, GC barriers и JIT ABI не менялись.

## Граница динамической загрузки модулей

В `module_ops.rs` подтверждены две ошибки:

- `run_import_namespace_dynamic_operands` использовал lossy `coerce_to_string`;
  `evaluate_import_call_options` делал то же для `type`. Запрос `./\ud800.js`
  действительно загружал существующий `./�.js`. Подмена происходила до lookup
  в `module_resolution_target` и передачи запроса `DynamicImportLoader::schedule`.
- Проверка типа/поддержки attribute шла внутри перечисления свойств. Например,
  `{ type: 42, get later() { throw marker; } }` отвергалась с `TypeError`,
  не вызывая getter. [EvaluateImportCall](https://tc39.es/ecma262/#sec-evaluate-import-call)
  сначала завершает `EnumerableOwnProperties`, затем проверяет attributes.

VM теперь использует точный `ToString`, сохраняет результат checked UTF-8
конвертации в Rust-owned памяти и откладывает отказ до чтения options. Все
attribute values сначала читаются в handle scope; следующий getter или Proxy
trap может собирать heap. Ошибки проходят через существующий единый обработчик
`CommittedValueError` → rejected Promise: удалены вложенный `Result<Result<…>>`
и дублирующая ветка создания Promise. Production-файл стал на 8 строк короче.
Новых crates, зависимостей, public API, dispatch layers и изменений GC/JIT ABI нет.
Сбор enumerable values добавляет временный список handles на холодном пути
`import(..., options)`; это требование observable порядка, не оптимизация скорости.

`module_request_identity.rs` проверяет prelinked и on-demand загрузку, high/low
lone surrogates, настоящий U+FFFD, emoji, повторный namespace cache hit, точное
число coercion calls, dynamic literal, JSON attribute через collecting Proxy,
приоритет thrown object и фильтрацию symbol/non-enumerable keys. Оба новых теста
сначала упали на исходной реализации; затем прошли. Static-request проверки из
`oxc_string_values.rs` сохранены. Запрет lone surrogates относится к UTF-8 host
request API Otter, не к JS-строкам вообще. `import.meta.resolve` в этом шаге не менялся.

| Проверка | До | После |
|---|---|---|
| Две новые runtime-регрессии | 2 failed | 2 passed |
| Test262 `language/expressions/dynamic-import` | 724 pass, 217 skip | 724 pass, 217 skip |
| Точные Test262 path → outcome | 941 строки | Все совпали |
| GC stress 1–16: identity, OXC strings, module error builders | Не запускался | 144 passed |

Условия Test262 одинаковы: macOS ARM64, pinned beta-2026-10-18, dev profile,
production-tiered, 4 workers, timeout 10 s, один прогон каждого состояния.
Skip policy не менялась; 217 skipped не считаются проверенными. Reports содержат
SHA-256 runner, revision корпуса и semantic config. Это проверка корректности,
не сравнение производительности. Build timings здесь не повторялись: граф
зависимостей не изменён, ускорение не заявляется.

Проверки: VM unit **1305 passed**; семь focused runtime suites **28 passed**;
dynamic-import capability и Web module suites **45 passed** (loopback sockets
разрешены отдельно); default CLI и Test262 runner собраны; workspace Clippy
`--all-targets --all-features -- -D warnings`, formatting и `git diff --check`
прошли. Linker сообщил о размере debug `__eh_frame`, Cargo — о future-incompat
в `proc-macro-error2`; они не замаскированы как ошибки этого изменения.

`jit_module_ops::synchronous_module_ops_match_interpreter_with_observable_effects`
падает на `reentrant > 0` **до и после**: semantic output совпадает, fixture не
входит в требуемый переход. Before воспроизведён в изолированном
`/tmp/otter-regex-boundary/source-before` с собственным target; test и исходный
`module_ops.rs` byte-identical, что проверено `cmp`. Assertion не удалён и не
ослаблен. Полный closing gate и runtime на других архитектурах не запускались;
этот шаг нельзя считать полностью зелёным workspace gate.

Журналы, исходный `module_ops.rs`, environment, Test262 reports/comparison и
строковые результаты stress сохранены в `/tmp/otter-module-boundary/`.
Основные команды (target вне пользовательского cache):

```sh
export CARGO_TARGET_DIR=/tmp/otter-architecture-review/target-migration
cargo test -p otter-runtime --test module_request_identity --test oxc_string_values
cargo test -p otter-runtime --test module_dynamic_import_capability --test web_modules
cargo test -p otter-runtime --test gc_module_error_builders --test typed_module \
  --test module_cycle_and_lifecycle --test module_source_embedding --test module_top_level_lexical
cargo test -p otter-vm --lib
cargo build -p otter-cli -p otter-test262
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
$CARGO_TARGET_DIR/debug/otter-test262 run --filter language/expressions/dynamic-import \
  --jobs 4 --timeout 10000 --output /tmp/otter-module-boundary/test262-after.json
```

## Восстановленная проверка native module reads

Причина падения `jit_module_ops` установлена: `hot(80)` не оплачивает стоимость
компиляции по текущей `TierWorkModel`. В исходном запуске Template было **0
compile attempts, 0 generations, 0 JIT events**. При `hot(1000)` появилась одна
Template OSR generation именно `hot`, после чего module transitions исполнялись.
Production policy и код VM/JIT не менялись; это дефект устаревшего fixture,
а не обнаруженная ошибка backend.

Тест теперь проверяет больше, чем положительный общий счётчик:

- После нормального прогрева находит installed/current/linked Template generation
  по `function_id`, полученному из реального source trace.
- После полного GC изменяет live export через функцию другого модуля.
- Повторный вызов читает новое значение и даёт тот же результат, что interpreter.
- В измеряемом вызове нет ни одного interpreted opcode `hot`; shared reentrant
  counter растёт, а исходный `code_object_id` остаётся installed/current/linked.
- Interpreter oracle действительно исполняет `LoadImportBinding`: tracer не
  может молча выключиться и дать ложное доказательство native execution.

Native proof относится к повторным binding reads. Одноразовые namespace imports,
star re-export, deferred evaluation и `import.meta.resolve` по-прежнему проверены
на observable семантику, но их native execution этот fixture не доказывает.
Удалён отдельный console-capture harness: сравнение идёт по owned completion.
Это не сокращение общего LOC: assertions точного исполнения увеличили тест;
новых tests, crates и production abstractions нет.

Результат: исходный тест failed → passed; GC stress **1–16 passed**; четыре
module suites **12 passed**; `scripts/gate.sh --quick all` passed; workspace Clippy
`--all-targets --all-features -- -D warnings`, formatting, `bash -n` и diff check
прошли. В full gate включён отдельный запуск `jit_module_ops`, `module_request_identity`
и `gc_module_error_builders` (**8 passed** той же командой).

Проверено на macOS ARM64, прежний pinned toolchain и dev profile. Target —
`/tmp/otter-architecture-review/target-migration`, журналы и исходный fixture —
`/tmp/otter-jit-module-review/`. Сборка и runtime не ускорялись и не измерялись:
изменены только тест и gate. Full closing gate, другие платформы и Test262
повторно не запускались. Ранее найденное падение `jit_module_ops` закрыто;
это само по себе не означает, что весь workspace gate зелёный.

## Поддержка static import attributes и владелец ошибок загрузки

`module_attribute_type` раньше искал первый `type` и игнорировал остальные
ключи. Static import с `extra` действительно выполнял зависимость и entry.
Теперь общий helper compiler/loader проверяет все ключи до сокращения AST
до `(specifier, attr_type)`. Диагностика указывает диапазон неподдерживаемого
ключа. Пустой clause, обычный import, `type` и его escaped spelling сохранены.
Новых зависимостей, DTO, форматов и public API нет.

Сохранено различие из [InnerModuleLoading](https://tc39.es/ecma262/#sec-InnerModuleLoading):
неподдерживаемый ключ static request даёт `SyntaxError` при загрузке. Options
самого `import()` по-прежнему дают `TypeError` после чтения всех getters.
Загрузка через `import()` модуля с неверным static request отвергает Promise
с `SyntaxError`. Это одна политика поддерживаемых ключей, не один класс ошибок.

Обнаружена связанная ошибка `handle::discover_remote_requests`: advisory prefetch
немедленно возвращал parse/request diagnostic из literal dynamic target,
не позволяя JS поймать её. Такие diagnostics теперь оставлены основному
`ModuleGraphBuilder`, который уже различает обязательные и optional dynamic
узлы. I/O, resource и cancellation errors этим изменением не подавляются.
Изменения только на frontend/загрузочной границе; GC barriers и JIT ABI не тронуты.

Проверки через public compiler entry points покрывают приём поддерживаемых
clause и точный span ошибки. Runtime проверяет 32 static комбинации (включая
re-export, defer, отсутствующую цель, неизвестный ключ до/после `type`), отсутствие
выполнения отвергнутого графа, literal/on-demand import и оба класса ошибок.
Async fixture проверяет финальный JS marker через отдельный host call, а не
только успешный возврат `run_module`. Усиленные регрессии падают на исходном
helper/handle и проходят после исправления. Существующий build fingerprint
охватывает `otter-syntax/src`, поэтому старые compiled-cache entries не
переиспользуются; версия внутреннего формата не добавлялась.

| Проверка | До | После |
|---|---|---|
| Test262 `import-attributes` | 83 pass, 13 skip | 83 pass, 13 skip |
| Test262 `language/expressions/dynamic-import` | 724 pass, 217 skip | 724 pass, 217 skip |
| Точные path → outcome обоих наборов | 972 уникальных пути | Все совпали |
| Новые static/async регрессии | Failed | Passed |

Test262: один прогон каждой конфигурации, macOS ARM64, dev profile,
production-tiered, 4 workers, timeout 10 s, прежняя skip policy. Наборы
пересекаются; их totals нельзя складывать. Compiler **146**, syntax **11**,
focused runtime/module suites **69 passed**; GC stress 1–16 — **192 passed**.
Default CLI/Test262 build, workspace Clippy all-targets/all-features,
formatting и diff check прошли. Loopback sockets для loader tests разрешены
отдельно. Журналы и comparison JSON: `/tmp/otter-import-attributes/`.

Граф зависимостей и горячие пути не изменены: ускорение сборки/VM не заявляется,
build timings не повторялись. Полный workspace/GC closing gate и runtime на
других платформах не запускались. Известные предупреждения debug linker и
future-incompat `proc-macro-error2` сохраняются.

## Подтверждённые следующие задачи

1. **P1: согласовать parse goals/scanner diagnostics.** Phantom/asm используют
   Parser напрямую; extract_specifiers посещает AST с parser errors.
   Приёмка: тесты malformed JS/TS и корректный контракт частичного результата.
2. **P1: повторить build measurements на свободном host.** Не менее трёх
   чередующихся before/after clean builds, отдельный linking measurement,
   восстановительный build между incremental triggers. Не обещать −50% LOC
   без перечня реально устраняемых обязанностей и сохранённых проверок.
