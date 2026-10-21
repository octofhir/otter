# Otter: продолжение работы с GOAL на конкурентный движок

Ты продолжаешь реальную разработку Otter в
`/Users/alexanderstreltsov/work/octofhir/otter`. Нужны большие законченные изменения
движка и доказанный результат относительно Bun. Node — дополнительный ориентир.
Не ограничивайся аудитом, планом или очередной небольшой оптимизацией.

## Установи GOAL в начале работы

Это явная инструкция создать активный GOAL через доступный инструмент управления
целями. Сначала проверь существующую цель: если она уже покрывает эту задачу,
продолжай ее; если незавершенной цели нет, создай следующую. Не подменяй создание
цели записью слова GOAL в Markdown. Бюджет токенов не задавай: я его не указал.
Если уже есть незавершенная цель с другой задачей, не закрывай ее фиктивно ради
создания новой. Сообщи о конфликте и продолжай полезную работу в разрешенном
объеме, не утверждая, что новый инструментальный GOAL установлен.

**Objective:** «Завершить архитектурный рефакторинг активного движка Otter по
R/F/N/O/C/X, довести текущий незакоммиченный cut-over до проверенного состояния,
реализовать нативный Graph на AArch64 и x86_64 и добиться измеренного превосходства
над Bun на заранее зафиксированной матрице исполнения, запуска, компиляции,
памяти и GC, сохранив ECMAScript-семантику и безопасность. Подтвердить результат
воспроизводимыми проверками, исходными замерами и таблицами по каждому случаю».

Цель остается активной, пока обязательные части не выполнены. Готовый код,
пройденный микробенчмарк и написанный отчет сами по себе не завершают GOAL.
Если инструмент целей недоступен, прямо сообщи об этом и продолжай работу;
не утверждай, что инструментальная цель установлена.

## Полномочия и темп

- Я разрешаю любые внутренние breaking changes: API, ABI, bytecode, layout,
  diagnostics, artifacts. Меняй текущий контракт и одновременно все его
  producer/consumer/test/doc. Удаляй замененный механизм в том же патче.
- Можно изучать V8, JavaScriptCore, SpiderMonkey, Bun и другие движки. Пользуйся
  локальными checkout или первичными источниками, фиксируй revision и изучаемый
  инвариант. Реализацию пиши самостоятельно с учетом GC и границ Otter.
- Работай автономно в разрешенном объеме. Не спрашивай повторное разрешение на
  рефакторинг, обычные исправления, удаление замененного кода или проверки.
- Делай крупные согласованные пакеты: несколько связанных вертикальных срезов
  от VM и compiler до обоих backend, runtime proofs и документации. Размер
  diff не является метрикой успеха; пакет должен удалять реальные источники
  затрат и доводить все затронутые границы до рабочего состояния.
- Используй доступные параллельные агенты, обычно основной агент и три
  исполнителя. Раздавай непересекающиеся файлы или точные участки общих файлов.
  Основной агент владеет интеграцией и единолично запускает Cargo и замеры.
- Начинай с нужных regression tests. Полный gate запускай после завершения
  согласованного пакета. Не повторяй весь gate после каждого мелкого исправления
  и не запускай тесты параллельно с записью исходников в этот же checkout.
- Кратко сообщай о результатах и следующем шаге примерно раз в минуту. После
  compaction продолжай текущую работу по сохраненному состоянию.

## Сначала восстанови фактическое состояние

Прочитай `AGENTS.md`, `ES_CONFORMANCE.md`, `PROMPT_HARD_REFACTOR_2.md` и
`benchmarks/results/refactor-rf/SESSION_STATUS.md`, затем проверь HEAD, index,
рабочий diff, незавершенные процессы и свободное место. Разделы handoff в старом
промте описывают разные моменты: текущий source и последний SESSION_STATUS
важнее старых утверждений «не реализовано» или «прошло».

Снимок на 2026-10-04, Москва:

- `main`, HEAD `5f1c195cbf721de7e1dcff7ad9b09b385df917c2`; большой общий WIP,
  включая уже staged удаление `machine/`. Не начинай заново и не перезаписывай
  чужой или предыдущий WIP. Ничего из этой новой волны еще не закоммичено.
- Нативный Graph существует на AArch64. x86_64 пока выполняет Template.
  Проверки target-neutral register contract на x86_64-модели не доказывают,
  что нативный x86_64 Graph backend уже существует.
- Предыдущие R/F targeted proofs прошли на предыдущем состоянии исходников.
  Они не подтверждают текущую расширенную волну N/O/C/X-foundation.
- `final-checks-5` прерван по запросу на ускорение, EXIT 137. Его завершенные
  шаги полезны как история, весь запуск не прошел.
- `parallel-nocf-1` завершился EXIT 101 на компиляции VM: отсутствовало поле
  allocation snapshot и были перемещения `Local` из borrowed destructuring.
  После этого исходники существенно изменились. Найди актуальное состояние
  этих мест; старую диагностику нельзя объявлять ни исправленной, ни актуальной
  без новой сборки.
- Текущая большая волна не собрана и не прогнана. Сначала доведи миграции
  ниже до целостного состояния, заморозь всех писателей и собери ее.
- К моменту этого handoff все три исполнителя подтвердили остановку записей.
  Root тоже прекратил engine edits; Cargo/rustc в process inventory не обнаружены.
  Выбранные файлы прошли syntax formatting, общий `git diff --check` прошел.
  Это не доказательство сборки или корректного исполнения.
- Checked all-corpus GC acceptance с 100 повторами, полный текущий conformance
  и сравнение текущего бинарника с Bun еще НЕ выполнены. Победа не доказана.

## Незаконченная волна: первый пакет продолжения

**N — литералы и young slabs.** Уже записаны empty LAB allocation, source-realm
allocation, `JitLiteralAllocationPlans` и native `NewObjectLiteral` /
`NewArrayLiteral`, canonical cold span. Bulk inputs используют канонические
homes; output появляется после точки, способной собрать мусор. Малые element
slabs переводятся на существующий young allocator. Заверши unit/runtime
доказательства, проверку shape/capacity/realm plans и всех allocation callback
signatures. Особенно проверь relocation cached slab base, adoption/growth
old-owner→young-slab, write barriers, dirty intervals, inputs через collection
и публикацию двух LAB cells только после полного initialization. Не сохраняй
Word pointer на moving slab через собирающий вызов.

**O — постоянный inline prefix.** Записан единый banked `FieldLocation`, capacity
на immutable shape, roots по `(prototype, capacity, kind)`, inline prefix и
slab только для suffix. Обновляются mutator, GC, CacheIR, constructor plans,
оба backend, Graph `LoadFieldBase` и artifacts. Проверь все потребители,
capacity 0/4/64, переход через границу, dictionary delete/descriptor/prototype
transitions, suffix growth и moving children в обеих банках. Header-kind/flags
cleanup и slack tracking еще требуют работы; весь раздел O пока не завершен.

**C/F — вызовы и поколение кода.** Записаны Template known method chains и
constructors на x86_64, actual-only argument ABI обоих backend и VM trampoline:
caller передает только actuals, callee владеет missing formals. Graph underarity
публикует необходимое окно до collecting helpers. Удаляются obsolete forwarding
API и caller-owned padding metadata. Проверь generic/known/forward/tail/bound/
construct, defaults/rest/mapped arguments, odd alignment slack, source realm,
exceptions и moving GC. Stale exit принадлежит точному вышедшему codeID:
диагностика и новое обучение сохраняются, неизменившийся старый exit не должен
ретайрить установленную replacement generation или отключать ее OSR.

**R/X-foundation — единые frame и register contracts.** Записаны
`graph/registers.rs`, `graph/frame.rs`, `graph/emission.rs`, параметризованный
allocator и `InputPolicy::Home`. Из deopt удален мертвый register-dump путь:
recipe использует canonical homes, native deopt entry теперь имеет пять
аргументов. Проверь все ABI/fixtures/artifacts, Windows hidden-result calling
convention, representation writeback и pressure на обеих register-моделях.
Сохрани CFG liveness, eager/lazy recipes, root regions и точное home expiration.
Потом реализуй настоящий x86_64 Graph emitter на этих общих владельцах.

Новые runtime binaries, которые надо включить в проверку:
`jit_empty_allocations`, `jit_generation_exit_ownership`,
`jit_template_direct_calls`, `jit_actual_only_calls`,
`jit_persistent_inline_fields`. Для N прочитай точный handoff
`benchmarks/results/refactor-rf/n-nonempty-source-handoff.md`: native unit
fixtures написаны, runtime proof непустых литералов и static-literal source-realm
stress fixture еще надо добавить. Существующие `jit_canonical_homes_gc`,
`jit_canonical_homes_retention`, `jit_source_work`, constructors, forwarding,
arguments и stack-owned array tests тоже защищают затронутые границы.

Не ослабляй доказательства ради зеленого результата: требуй собственный
реально исполненный native generation, соответствующий artifact/call edge,
проверку результата и настоящее collection/reentry. Устаревшую проверку
удаленного механизма перепиши на его замену, сохранив смысл регрессии.

## Следующие большие пакеты

После интеграции текущей волны выбирай порядок по измеренной стоимости и
зависимостям. Не делай все последовательно, если границы уже позволяют параллель.

1. Заверши N/O: closure/constructor allocation, allocation folding, native
   string concatenation и сравнения, typed runtime calls, shape transitions,
   удаление header flags и constructor profile, безопасный slack tracking.
2. Заверши C: current entry-cell linkage, дешевые known-kind entries,
   target-specific argument conventions, return-address→code-object PC tables
   для inspection вместо лишних hot-path stamps. Сохрани одну frame/root модель.
3. Реализуй X: instruction selection, moves, calls, safepoints, deopt, OSR и
   artifacts для native x86_64 Graph, без копирования ARM runtime. Учитывай
   two-address instructions, RAX/RDX/CL, stack alignment и callee-save ABI.
4. Параллельно устраняй большие измеренные затраты запуска, frontend,
   TypeScript/module execution, GC и памяти внутри активного стека. Включай
   реальные короткие и длинные программы; не оптимизируй только семь kernels.

Числовые Node milestones из старого промта остаются промежуточными проверками.
Основной конкурентный критерий — Bun на текущих одинаковых workload hashes.

## Матрица победы и честные замеры

До конкурентных замеров зафиксируй workload/platform/metric matrix и условия
прохождения. Включи семь existing fixed-work workloads, cold CLI/startup и first
useful result, warm throughput, compilation cost, реальные TS/module программы,
mono/poly/uncacheable calls/properties, allocation pressure, peak RSS, retained
heap после полного GC и GC pause distributions. Разделяй стоимость исполнения
движка и поддерживаемые end-to-end runtime сценарии; неподдержанный сценарий
записывай как coverage gap. После плохих результатов не сокращай матрицу.

Победа в случае означает лучший целевой показатель относительно Bun вне
измеренного шума при одинаковом полезном результате. Покажи все показатели и
проигрыши; geometric mean не доказывает превосходства в каждом случае. Не
считай снижение instructions доказательством меньшего elapsed time, RSS —
retained heap, а проверку Rosetta/qemu — native performance.

- Запиши source revision/dirty provenance, точные binaries/scripts hashes,
  версии Bun/Node, архитектуру, build profile и режим исполнения.
- Используй одинаковый алгоритм, данные и проверку выхода. Crypto RNG и zlib
  validation уже изменены: старые timings для них не являются равной работой.
- Минимум пять измеренных повторов, независимые fresh-process cold samples,
  отдельный настоящий warm harness, rotation engine order, median и spread.
  Если выигрыш перекрывается шумом, увеличь число повторов и проверь причину.
- Измеряй idle host, без Cargo и profiling. Не выключай чужие приложения.
  GC tails и retained memory измеряй отдельными подходящими harnesses.
- Новый Otter сравни с Bun/Node и сохраненным Otter там, где provenance позволяет.
  Старый бинарник с неизвестной связью с source не объявляй чистым baseline.
- Результаты, команды, manifests и raw samples сохраняй в
  `benchmarks/results/<fresh-dir>/`. Используй готовые drivers после проверки
  их соответствия текущей волне; не выдавай synthetic harness tests за engine runs.

## Обязательные проверки и границы

- Только активный стек `otter-gc → otter-vm → otter-runtime → product crates`.
  Никаких parallel engines, parked dependencies, compatibility readers,
  migrations, duplicate carriers или обходов новой границы через старую.
- Поиск в git-indexed репозитории через `fff`; `rg` — только при недоступном
  сервисе. JS/TS разбирай AST. Новые module docs и runtime/types/docs держи
  согласованными. Native allocations — через штатные scopes/NativeCtx;
  соблюдай GC/barrier invariants и deny-by-default capabilities.
- Работай на `main`, без branch/worktree/stash/reset/checkout/restore поверх WIP.
  Не форматируй целиком общие файлы, пока другие агенты пишут в них.
- Не вводи compatibility switches, новые TLS/static mut или benchmark-specific
  правила. Diagnostics остаются default-off, без лишних hot-path allocations.
- Один владелец Cargo. Максимум один-два runtime test binaries за запуск;
  `cargo test --workspace` запрещен. Один Test262 runner, jobs 1, timeout 30000 ms.
- Проверки запускай с явным watchdog и проверкой exit status, полноты результатов,
  source/binary/corpus identities. Отсутствующий результат не означает Pass.
- На цельном замороженном состоянии: fmt, VM/JIT tests, оба backend, затронутые
  runtime proofs, clippy с `-D warnings`, checked difftest по всем случаям и
  checked GC stress 1–16 с heap verification. Для приемки R — весь актуальный
  difftest corpus по 100 повторов на каждый stride, без ослабления этого gate.
- Test262 `language/` без новых failures; `built-ins/` без новых failures сверх
  двух прежних RegExp budget failures. Сверяй exact identities/skip causes,
  а не только totals. Полная публикация требует полного canonical capture;
  historical aggregate report не превращается в новый полный отчет.
- Перед большим build проверь диск. Удаляй только идентифицированный disposable
  cache, когда его никто не использует; binaries и raw evidence сохраняй.
- Commit только после обязательных gates, GPG-signed, отдельным вызовом.
  Исключи `PROMPT_*.md`, `benchmarks/measurements/`,
  `scripts/dev/t262_tostring_probe.js` и локальные результаты. Не добавляй AI trailers.

Полезные drivers в `benchmarks/results/refactor-rf/`:
`run-final-checks.py`, `build-final.py`, `gate-final.py`, `gc-shards.py`,
`test262-old-workers.py`, `fixed-work.py`. Проверь их CLI и обнови список тестов
для текущего пакета. Сохраненные abort/failure logs не переименовывай в успех.

## Когда можно завершить GOAL

Все обязательные R/F/N/O/C/X механизмы реализованы и старые пути удалены;
полные обязательные correctness/GC/conformance gates прошли на финальных
исходниках; нативные платформы проверены; заранее объявленная матрица
подтверждает превосходство над Bun по каждому заявленному целевому показателю;
результат воспроизводим из сохраненных команд, manifests и raw samples.
Пока есть обязательный непройденный случай, проигрыш или неподтвержденная
платформа, GOAL не complete. Не обещай универсальную победу на любых программах:
границы подтвержденного результата всегда называй точно.

Финальный результат: конкретные изменения архитектуры, удаленные механизмы,
прошедшие gates с путями к logs, per-case before/after/Bun/Node таблицы,
оставшиеся ограничения и следующий самый дорогой измеренный bottleneck.
Начинай с завершения текущей волны. Не заканчивай ответ планом — делай работу.
