# Element store bounds feedback: a growing store site is learned once

Parent `ef556eae7`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`, profiles
`benchmarks/results/gbemu-prof/plain*.txt`, JIT events
`benchmarks/results/gbemu-prof/ev*.json`.

## Удалённый механизм

Цикл деоптимизаций по одному сайту за раз. Оптимизирующий тир для каждого
`StoreElement` с known-семейством спекулировал «индекс внутри элементов»
(`CheckBounds`), а обратная связь базового тира хранила только семейство
получателя — не то, что запись уходила за длину (append, дырка). Сайт учился
только на собственном деопте (`exited_for(BoundsGuard)` — по pc). В gbemu
функции заполнения таблиц памяти (`this.memoryWriter[index] = …`, несколько
веток — несколько сайтов, каждый вызов с новым массивом) деоптировали на
очередном новом сайте при каждом повторном входе: fid 1337 — 11 деоптов
`boundsGuard` и 12 перекомпиляций, fid 1283 — 8 и 9, с 330 по 965 мс прогона;
между ними цикл шёл в template/интерпретаторе (интерпретатор — 32% главного
потока).

Теперь запись за пределами плотных элементов (Array: индекс ≥ dense length;
typed array: индекс ≥ length) ставит в ячейке обратной связи сайта бит
`ELEMENT_OUT_OF_BOUNDS_SEEN` — из runtime-пути базового тира и из
интерпретатора, с продвижением эпохи. Снимок перечисляет такие сайты
(`out_of_bounds_element_sites`), и оптимизирующий тир сразу берёт для них
общий keyed store (дописывание идёт через `ARRAY_PUSH_LEAF`), без
in-bounds-спекуляции; template сохраняет свой in-bounds fast path. Чтения не
помечаются: однократное чтение за концом (выход из цикла) иначе навсегда
сделало бы общим горячий сайт чтения.

После: в gbemu 0 деоптов `boundsGuard`, интерпретатор 32.3% → 25.8%.

## Wall-clock (медиана 5; база — `guarded-method-cells/wall-final`)

| Бенч | otter до | после | o/node до | после | RSS MB |
|---|---:|---:|---:|---:|---:|
| richards | 1249 | 1247 | 1.96 | 1.96 | 52 |
| deltablue | 1383 | 1376 | 4.99 | 4.89 | 106 |
| crypto | 239 | 238 | 1.55 | 1.54 | 45 |
| raytrace | 1499 | 1461 | 2.82 | 2.74 | 104 |
| earley-boyer | 1611 | 1610 | 1.55 | 1.56 | 113 |
| regexp | 2198 | 2207 | 5.46 | 5.48 | 181 |
| splay | 709 | 708 | 2.41 | 2.40 | 577 |
| navier-stokes | 905 | 907 | 1.28 | 1.29 | 34 |
| box2d | 852 | 842 | 2.80 | 2.84 | 156 |
| gbemu | 1476 | **1216** | 3.03 | **2.59** | 245 |
| mandreel | 1223 | 1202 | 1.75 | 1.75 | 233 |
| zlib | 1916 | 1914 | 1.19 | 1.19 | 123 |
| ts | 3296 | 3293 | 2.91 | 2.88 | 543 |

gbemu −17.6%. Только время otter: геосреднее ×0.981. Геосреднее этого
прогона к node 2.32 → **2.29**; против усреднённых по двум прогонам node и
bun — 2.329 → **2.283** и 3.195 → **3.174**.

## Fixed-work instructions (диагностика; сумма по потокам)

gbemu 23.09G → **19.14G (−17.12%)**; остальные −1.28…+0.41%. Геосреднее
2.054343 → **2.022224** (−1.56%).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13e`, отчёт `full-2026-10-13f`. Difftest host/x86
163/0, VM 1250/0, JIT 270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13
и `gcfull` (13 фикстур + корпуса `element_store_growth`,
`typed_view_keyed_store`, `parallel_scavenge`) — вывод совпал. Корпус
`element_store_growth` (заполнение свежего Array дописыванием из нескольких
веток с повторным входом, разреженный рост с дырками, typed-array записи за
концом, дырки и заполнения у Array и объектов) совпал с Node в interpreter,
template, native и стресс-режимах 256 и 1; бинарник `ef556eae7` деоптирует на
нём 3 раза по `boundsGuard`, новый — ни разу.
