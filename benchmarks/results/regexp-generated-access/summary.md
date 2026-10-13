# RegExp access in generated code: prototype role before bake, `lastIndex` body slot, reused matcher buffers

Parent `bd989f4b2`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`, profiles
`sample-regexp.txt`, `sample-exec-null.txt`.

## Удалённые механизмы

- Сдвиг формы прототипа после выпечки программы. `%RegExp.prototype%`
  получал роль прототипа (новый hidden class) только при первом
  `replace`/`split`/`test` — их fast path берёт instance root ради
  протокольного доказательства. Программы, испечённые раньше, сторожили
  старую форму: guard промахивался, оптимизированный код деоптировал по
  `shapeGuard`, а перекомпиляция с `exited_for(ShapeGuard)` выключала
  все программы функции. В regexp так выпадал `Exec` целиком.
  Теперь `shaped_prototype_program` сам берёт instance root прототипа до
  чтения формы — для `%RegExp.prototype%`, `%Map/Set.prototype%` и
  `%Function.prototype%` одинаково; повторный root форму не меняет.
- Runtime-стаб на каждую запись `re.lastIndex = …`: RegExp — не
  ordinary object, у сайта не было программы, и каждая запись шла через
  `jit_store_property_stub` → `ordinary_set_data_value` (4 328 464 раз за
  прогон regexp, 27% вызова `Exec` на `/^ba/`). Теперь `lastIndex` —
  `Cell<Value>` с фиксированным смещением в теле, и общий CacheIR-слот
  тела (`LoadBodySlot`/`StoreBodySlot`: тег ячейки, guard слова тела,
  смещение) читается и пишется inline: запись — в оптимизирующем тире
  (`Kind::StoreBodySlot` + обычный WriteBarrier), чтение — в обоих тирах.
  Запись проверяет бит writable; промах — деопт до эффектов.
- Аллокация и рост буферов матчера на каждый поиск: `Matches` создавал
  пустой backtrack-стек, undo-лог и массив захватов, а `exec` на
  коротких строках платил malloc/free и рост `Vec` при каждом вызове.
  Теперь буферы — `MatchScratch` изолята (`Cell` в интерпретаторе),
  поиск берёт их по значению и возвращает (`Matches::into_scratch`);
  разросшиеся сверх 1024 записей отдаются пустыми.

Заодно: запись в non-writable `lastIndex` в интерпретаторе молча
игнорировалась и в strict-коде не бросала `TypeError` (JIT-тиры бросали).
Новый корпус `regexp_last_index_slot` это поймал; путь исправлен.

## Wall-clock (медиана 5; база — `string-method-programs/wall-final`)

| Бенч | otter до | после | o/node до | после | RSS MB |
|---|---:|---:|---:|---:|---:|
| richards | 1309 | 1311 | 2.06 | 2.07 | 52 |
| deltablue | 1462 | 1481 | 5.13 | 5.31 | 106 |
| crypto | 238 | 240 | 1.55 | 1.54 | 46 |
| raytrace | 1484 | 1609\* | 2.79 | 2.88 | 104 |
| earley-boyer | 1608 | 1646 | 1.56 | 1.56 | 113 |
| regexp | 3006 | **2458** | 7.48 | **6.11** | 182 |
| splay | 695 | 711 | 2.14 | 2.38 | 578 |
| navier-stokes | 908 | 908 | 1.17 | 1.28 | 34 |
| box2d | 851 | 844 | 2.87 | 2.85 | 157 |
| gbemu | 1447 | 1444 | 3.11 | 3.09 | 255 |
| mandreel | 1200 | 1197 | 1.76 | 1.75 | 232 |
| zlib | 1914 | 1914 | 1.19 | 1.19 | 124 |
| ts | 3273 | 3308 | 2.89 | 2.89 | 607 |

regexp −18.2%. Только время otter: геосреднее ×0.996. Node в этом
прогоне быстрее на splay (324 → 299) и navier-stokes (774 → 707), поэтому
геосреднее к node этого прогона 2.37 (было 2.36); против усреднённого по
двум прогонам node — 2.373 → **2.363**. К bun 3.38 → 3.37. \* raytrace
бимодален, инструкции те же (21.28G).

Микробенч `scripts/dev/probe/regexp_exec_null.js` (`Exec` на `/^ba/`,
провал на смещении 0): 172 → 126 нс на вызов (node 13, bun 21).

## Fixed-work instructions (диагностика; сумма по потокам)

regexp 45.03G → **36.41G (−19.15%)**. Геосреднее 2.138142 → **2.101316**
(−1.72%); остальные фикстуры −0.67…+0.15%.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13a`, отчёт `full-2026-10-13b`. Difftest host/x86
160/0, VM 1248/0, JIT 270/0, x86 293/0, otter-regex 23+46/0, clippy host/x86.
GC stress 256 всех 13 и `gcfull` (13 фикстур + корпуса
`regexp_last_index_slot`, `intrinsic_method_receivers`, `parallel_scavenge`) —
вывод совпал. Корпус `regexp_last_index_slot` (все виды флагов, подкласс,
значения всех типов в старых RegExp через сборки, non-writable `lastIndex`
в sloppy и strict, замороженный RegExp, ordinary-объекты с `lastIndex` на тех
же сайтах) совпал с Node в interpreter, template, native и стресс-режимах
256 и 1.
