# Plain proof loads: same-thread validity reads without acquire

Parent `e6d21b3b1`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`.

## Удалённый механизм

Acquire-барьер (`ldar`/`ldarb`) на каждом чтении слова-доказательства в
сгенерированном AArch64-коде: validity-ячейки прототипной цепочки (guard
наследуемого метода, append-переход в store, absent-key загрузки и записи
в общей таблице свойств, proof записи IC) и field-type слова
(`CheckFieldTypes`). Эти слова гасит/расширяет только мутатор, и
сгенерированный код исполняется на том же потоке мутатора — обычная
загрузка видит каждое предшествующее в программном порядке изменение.
Acquire же запрещал последующим загрузкам выполняться раньше, сериализуя
зависимые чтения объектов за guard-ом. Читатели в других потоках
(компилятор) остаются на Rust-атомиках с `Acquire`. На x86-64 загрузки и
так были обычными.

## Wall-clock (медиана 5; база — `element-store-bounds/wall-final`)

| Бенч | otter до | после | o/node до | после |
|---|---:|---:|---:|---:|
| richards | 1247 | 1250 | 1.96 | 1.97 |
| deltablue | 1376 | **1326** | 4.89 | **4.73** |
| crypto | 238 | 237 | 1.54 | 1.52 |
| raytrace | 1461 | 1447 | 2.74 | 2.72 |
| earley-boyer | 1610 | 1610 | 1.56 | 1.56 |
| regexp | 2207 | 2205 | 5.48 | 5.46 |
| splay | 708 | 706 | 2.40 | 2.36 |
| navier-stokes | 907 | 907 | 1.29 | 1.29 |
| box2d | 842 | 844 | 2.84 | 2.84 |
| gbemu | 1216 | 1220 | 2.59 | 2.59 |
| mandreel | 1202 | 1198 | 1.75 | 1.76 |
| zlib | 1914 | 1919 | 1.19 | 1.19 |
| ts | 3293 | 3284 | 2.88 | 2.87 |

deltablue −3.6%; остальные в разбросе. Только время otter: ×0.996.
Геосреднее этого прогона к node 2.29 → **2.28**; против усреднённых по двум
прогонам node и bun — 2.287 → **2.278** и 3.244 → **3.226**.

## Fixed-work instructions

Число инструкций не меняется (та же длина последовательностей):
геосреднее 2.022224 → 2.022533.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13f`, отчёт `full-2026-10-13g`. Difftest host/x86
163/0, VM 1250/0, JIT 270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13
и `gcfull` — вывод совпал. Корпуса `guarded_method_cells` и
`intrinsic_method_receivers` (переопределение/удаление методов прототипа
посреди горячих циклов гасит validity-ячейки) совпали с Node в interpreter,
template, native и стресс-режимах 256 и 1.
