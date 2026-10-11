# Проверка emitted code и fixed-work

ARM64 `f579/c70`: frame entry **33→22** инструкции; proven-receiver entry с `mov x5, xzr` — **34→23**. До: `[0x284,0x308)`, после: `[0x30c,0x364)`. Здесь нет циклов и обычный вызов проходит весь frame-участок, поэтому статический счёт совпадает с выполненными инструкциями этого пути.

| Обязательные операции frame entry | До | После | Ушло |
|---|---:|---:|---:|
| Store instructions | 13 | 7 | 6 |
| Load instructions | 3 | 1 | 2 |
| Branch instructions | 4 | 4 | 0 |
| Остальные инструкции | 13 | 10 | 3 |
| Всего | 33 | 22 | 11 |

Удалены 13 инструкций: zero scratch (1); load/save предыдущего frame (2); function id и header load/store (3); zero window metadata (1); actual-base и actual/parent store (2); root descriptor/base/store (3); публикация через `str x21,[x20,#8]` (1). Добавлены только `mov x16,#2` и сохранение маркера (2). Суммарно исчезли семь старых stores и два loads, добавился один marker store. Проверка стека и continuation/staged contract сохранились.

Обычный возврат **11→8** выполненных инструкций, когда вызов сохранил unpublished marker. До: status, load return anchor, construct-bit check, tier-frame compare/branch, parent publication, restore/return. После: status, load marker, taken `tbnz w9,#1`, restore/return. Construct check, tier compare/branch и parent store пропускаются; новый bit-one branch заменяет четыре инструкции. Net: один store, одна branch и одно сравнение исчезли; loads сохранились. Точные пути:

```text
before: 4cc,4d0,4d4,4d8,4dc,4f0,4f4,4f8,4fc,500,504
after:  528,52c,530,554,558,55c,560,564
```

Вместе proven-receiver entry и return: **45→31**, или **−14 инструкций / −31.1%** для этого пути. Net memory/control reduction: семь stores, два loads и одна branch. Это не оценка всех вызовов: collecting/helper, construct, underarity и reconstruction публикуют необходимый frame на своей границе. В `f579` exact publication spans: `[0x68,0xac)`, `[0x244,0x288)`, `[0x7a4,0x7e8)`, `[0x8b8,0x8fc)`. В after self-профиле samples внутри них отсутствуют. Холодная работа перенесена, а общий code object вырос `2320→2576` bytes. У `f548/c30` frame entry остался 65 emitted instructions: его window-backed путь не получил сокращения.

Профили: before 2350 self samples, generated 1557; after 2348 и 1595. Private entry: `179/11.5%→167/10.5%` generated; обычный return: `51/3.3%→43/2.7%`. Это sampled time, отдельное доказательство retired instructions дают fixed-work counters. Inclusive rows нигде не учитывались.

Единственный прежде неразмеченный after sample — ровно одна строка self: `JS:<anonymous> [template c118]+0x1541`, функция `f552`. После `& ~3` получается `0x1540`: `stp x29,x30,[sp,#-0x30]!`. Он лежит в authoritative `code-map.json.osrEntries[logicalPc=4]`, `[0x1540,0x1568)`: это **OSR tier prologue**, а не возврат по предыдущему `ret` на `0x153c`. Инструкция пересечения границы не допускает `+4` tolerance. Script обновлён: отдельные интервалы `osrEntries` включены в разметку. Повторная проверка дала **outside 0, OSR tier prologue 1**; before aggregate полностью прежний. Точный schema-фрагмент: `{"osrEntries":[{"logicalPc":4,"startOffset":5440,"endOffset":5480}]}`.

## Fixed-work: retired instructions

Точная база: `field-shape-tracking/gate-summary.json`, `performance.measurements`; новые counters: `performance.json`. Дельты вычислены по целым counters, отображаемые G округлены. Node counters совпадают для всех 13 бенчей.

| Бенч | До Otter, G | После Otter, G | Дельта Otter | После Otter/Node |
|---|---:|---:|---:|---:|
| richards | 28.739 | 28.506 | -0.810% | 2.466× |
| deltablue | 40.301 | 39.906 | -0.980% | 7.138× |
| crypto | 4.684 | 4.676 | -0.183% | 1.602× |
| raytrace | 35.095 | 34.648 | -1.275% | 4.139× |
| earley-boyer | 29.688 | 29.688 | +0.001% | 1.589× |
| regexp | 51.134 | 51.043 | -0.176% | 6.990× |
| splay | 12.217 | 12.189 | -0.231% | 2.772× |
| navier-stokes | 8.892 | 8.885 | -0.075% | 1.098× |
| box2d | 14.257 | 14.191 | -0.458% | 2.803× |
| gbemu | 26.505 | 26.320 | -0.699% | 3.202× |
| mandreel | 19.209 | 19.188 | -0.111% | 1.388× |
| zlib | 24.508 | 24.502 | -0.025% | 0.990× |
| ts | 37.939 | 37.805 | -0.353% | 1.813× |

Геометрическое среднее: **2.411100→2.401113× (-0.414%)**. Точная deltablue база: 40.301380411G, final: 39.906448842G, **-0.980%**. Отдельный свежий before 40.34G даёт примерно −1.07%, но только этот дополнительный counter округлён.

Явная регрессия: **earley-boyer +0.001%** (29,687,581,279→29,688,018,173; +436,894 retired instructions). Все остальные 12 counters снизились. Такой малый рост измерен, его причина этим замером не установлена. Splay 12.216988547→12.188773105G (−0.231%) всё ещё выше до-field-tracking базы 11.99G: прежняя регрессия сохраняется примерно на 1.66%.
