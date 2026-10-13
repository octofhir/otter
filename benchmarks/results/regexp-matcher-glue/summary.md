# RegExp matcher and exec glue: anchored reject, capture reset, frameless coercions, one element-cache refresh

Parent `dd4a6b58c`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`, profiles
`sample-regexp.txt`, `sample-exec-null.txt`, `sample-re1.txt`.

## Удалённые механизмы

- Полная попытка матчера для start-anchored паттерна, чей первый символ
  не может начать совпадение: сканер `Anchored` не смотрел на уже
  посчитанный first-set prefilter. `/^ba/` на `pyvpx` строил `Matcher`,
  сбрасывал захваты, пушил фрейм и падал на первом `Char`. Теперь один
  тест первого кодпоинта.
- Пересборка массива захватов на каждую попытку (`clear` + `resize`
  поэлементно) — теперь `fill(None)` при той же длине.
- Handle-фрейм на каждую ToString/ToLength уже строкового/числового
  значения и внешний scope `RegExp.prototype.exec` над строковым
  аргументом: обёртки открывали по фрейму на вызов.
- Копия `Vec` захватов на каждый успешный `exec` только чтобы передать
  срез в legacy-статику `RegExp.$1…`.
- Обновление element cache массива (`elements_ptr`, `dense_cap`,
  `dense_kind`, own-guard) после **каждого** элемента при сборке массива
  из готовых значений (`copy_values_into` — путь всех runtime
  `array_from`: результаты `exec`, `split`, `match` и пр.). Теперь одно
  обновление на копию.
- Спилл в кучу `SmallVec` на 8 при 9+ захватах (две аллокации на `exec`
  URL-паттерна с 11 группами): ёмкость 16.

Найдено корпусом под GC-stress 1 и исправлено: активный realm хранит
subject legacy-статики (`RegExp.$1`, `lastMatch`, `lastParen`, `input`)
в `Interpreter.regexp_legacy`, но корневой обход трассировал только копии
в припаркованных `RealmState`. После сдвигающей сборки статика читала
чужую строку. Теперь обход корней ведёт и активную копию (и `root_census`).

## Wall-clock (медиана 5; база — `regexp-generated-access/wall-final`)

| Бенч | otter до | после | o/node до | после | RSS MB |
|---|---:|---:|---:|---:|---:|
| richards | 1311 | 1309 | 2.07 | 2.06 | 52 |
| deltablue | 1481 | 1461 | 5.31 | 5.11 | 106 |
| crypto | 240 | 239 | 1.54 | 1.56 | 46 |
| raytrace | 1609 | 1545 | 2.88 | 2.90 | 104 |
| earley-boyer | 1646 | 1610 | 1.56 | 1.56 | 113 |
| regexp | 2458 | **2207** | 6.11 | **5.14** | 183 |
| splay | 711 | 712 | 2.38 | 2.40 | 574 |
| navier-stokes | 908 | 907 | 1.28 | 1.29 | 34 |
| box2d | 844 | 848 | 2.85 | 2.86 | 157 |
| gbemu | 1444 | 1445 | 3.09 | 3.09 | 254 |
| mandreel | 1197 | 1196 | 1.75 | 1.75 | 233 |
| zlib | 1914 | 1910 | 1.19 | 1.19 | 124 |
| ts | 3308 | 3283 | 2.89 | 2.86 | 542 |

regexp −10.2%. Геосреднее к node 2.37 → **2.34**, к bun 3.37 → **3.33**.

Микробенчи (`scripts/dev/probe/regexp_exec_null.js`,
`regexp_exec_variants.js`): `exec` с провалом на смещении 0 —
126 → 81 нс (node 13), `test` 100 → 81 нс; `exec` URL-паттерна с 11
группами — 721 → 683 нс (node 118).

## Fixed-work instructions (диагностика; сумма по потокам)

regexp 36.41G → **32.99G (−9.39%)**. Геосреднее 2.101316 → **2.085997**
(−0.73%); остальные −0.47…+0.67%.

## Correctness

Попытка 1 гейта упала: difftest host и oracle stress-1 на новом корпусе
`regexp_exec_protocol` поймали неукоренённую legacy-статику (дефект
существовал и до среза — бинарник `dd4a6b58c` давал тот же вывод).
Попытка 2 после исправления: test262 53 575 / 52 463 / 12 fail / 1100
skip, 0 timeout/OOM/crash, failure set идентичен `full-2026-10-13b`, отчёт
`full-2026-10-13d`. Difftest host/x86 161/0, VM 1248/0, JIT 270/0, x86
293/0, otter-regex 23+47/0, clippy host/x86. GC stress 256 всех 13 и
`gcfull` (13 фикстур + корпуса `regexp_exec_protocol`,
`regexp_last_index_slot`, `intrinsic_method_receivers`) — вывод совпал.
Корпус `regexp_exec_protocol` (числовые формы `lastIndex`, нестроковые
субъекты, legacy-статика после 7–20 захватов, start-anchored паттерны
с case-folding, суррогатами и пустым субъектом) совпал с Node в
interpreter, template, native и стресс-режимах 256 и 1.
