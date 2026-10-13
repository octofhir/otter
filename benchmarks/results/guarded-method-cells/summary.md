# Guarded method cells: a proven inherited method is one load

Parent `0536d328f`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`, per-node-kind
profiles `kind-deltablue/`, `kind-richards/` (`scripts/dev/sample_kind_prof.py`).

## Удалённый механизм

Чтение унаследованного метода в оптимизированном коде
(`LoadGuardedMethod`) после проверки receiver shape и validity-ячейки
прототипной цепочки каждый раз заново шло к слоту: литерал root shape →
`prototype` из тела shape → база хранилища holder → слот (три зависимые
загрузки через разные кэш-линии). При этом снимок уже доказывал
(`slot_proven`), что пока validity-ячейка жива, слот держит ровно ту
замыкание, что было при выпечке: любая запись в наблюдаемый прототип
гасит ячейку. Теперь выпечка кладёт это замыкание в удерживаемую кодом
трассируемую ячейку (`bake_held_identity_cell`; сборщик переписывает её
при перемещении замыкания), и оба бэкенда графового тира после проверки
validity читают метод одной загрузкой из ячейки.

До: `LoadGuardedMethod` — 14.7% главного потока deltablue и 10.9% richards
(профиль по видам узлов). В горячем `Plan.prototype.execute` deltablue все
18 чтений методов теперь идут через ячейку (5 инструкций вместо 9–15).

## Wall-clock (медиана 5; база — `regexp-matcher-glue/wall-final`)

| Бенч | otter до | после | o/node до | после | RSS MB |
|---|---:|---:|---:|---:|---:|
| richards | 1309 | **1249** | 2.06 | **1.96** | 52 |
| deltablue | 1461 | **1383** | 5.11 | **4.99** | 107 |
| crypto | 239 | 239 | 1.56 | 1.55 | 46 |
| raytrace | 1545 | 1499 | 2.90 | 2.82 | 104 |
| earley-boyer | 1610 | 1611 | 1.56 | 1.55 | 113 |
| regexp | 2207 | 2198 | 5.14 | 5.46 | 182 |
| splay | 712 | 709 | 2.40 | 2.41 | 574 |
| navier-stokes | 907 | 905 | 1.29 | 1.28 | 34 |
| box2d | 848 | 852 | 2.86 | 2.80 | 157 |
| gbemu | 1445 | 1476 | 3.09 | 3.03 | 254 |
| mandreel | 1196 | 1223 | 1.75 | 1.75 | 233 |
| zlib | 1910 | 1916 | 1.19 | 1.19 | 124 |
| ts | 3283 | 3296 | 2.86 | 2.91 | 581 |

richards −4.6%, deltablue −5.3%. Только время otter: геосреднее ×0.993.
Геосреднее этого прогона к node 2.34 → **2.32**, к bun 3.33 → 3.13 (bun
шумит: box2d 268 → 376 мс); против усреднённых по двум прогонам node и bun
— 2.339 → **2.322** и 3.244 → **3.205**. gbemu и mandreel +2% — шум
(инструкции те же).

## Fixed-work instructions (диагностика; сумма по потокам)

deltablue 28.07G → **25.70G (−8.46%)**, richards 27.35G → **25.26G
(−7.66%)**, ts −1.40%, raytrace −1.30%, box2d −0.68%; остальные
−0.05…+0.22%. Геосреднее 2.085997 → **2.054343** (−1.52%).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13d`, отчёт `full-2026-10-13e`. Difftest host/x86
162/0, VM 1248/0, JIT 270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13
и `gcfull` (13 фикстур + корпуса `guarded_method_cells`, `background_compile`,
`parallel_scavenge`) — вывод совпал. Корпус `guarded_method_cells` (свежие
молодые замыкания-методы, перемещаемые сборками, пока код держит их ячейки;
переопределение и удаление метода прототипа посреди горячего цикла;
`setPrototypeOf` экземпляра; затенение собственным свойством) совпал с Node в
interpreter, template, native и стресс-режимах 256 и 1; ячейки используются в
10 оптимизированных телах корпуса.
