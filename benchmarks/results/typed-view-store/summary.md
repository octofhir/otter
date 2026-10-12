# Typed view stores inline; ToInt32 for 8/16-bit elements

Parent `0cc027bb9`. Machine provenance: [performance.json](performance.json),
[test262-validation.json](test262-validation.json), wall-clock
`benchmarks/results/wallclock-2026-10-12/` (тот же бинарь).

## Обязательная работа, которая исчезла

Generic keyed store в graph-тире знал inline только Array: любой store в
typed view (`Uint8Array`, `Float64Array`, …) уходил в committed runtime-
вызов — парковка трёх операндов, вход в Rust, разбор вида, конвертация,
запись. gbemu пишет в `Uint8Array`/`Int8Array` память эмулятора на каждом
такте.

## Архитектура

- `JitTypedArrayStorage` в `otter-vm/src/jit.rs`: тег ячейки и смещения
  `kind`/`length`/`data` typed view от `GcHeader`, один дескриптор для обоих
  backend'ов.
- ARM `graph/arm64/keyed_store.rs`, x86 `graph/x86_64/keyed_store.rs`: после
  промаха по Array-тегу — проверка тега view, int32 ключ ≥ 0, ненулевой
  кешированный base (fixed-length локальный буфер), detach protector цел,
  ключ < length. Значение — уже Number: int32 пишется низкими битами в
  8/16/32-битный вид, Uint8Clamped насыщается, double идёт через ToInt32
  (`fjcvtzs`, без него — отказ в runtime) или через `fcvt`/`str d` в
  Float32/Float64. BigInt и Float16 отказывают. Отказ ничего не пишет.
- VM: Int8/Uint8/Int16/Uint16, DataView-сеттеры и Atomics брали
  `trunc() as i64` и насыщали значения ≥ 2^63 вместо §7.1.6 ToInt32 по
  модулю 2^32. Теперь все пути — `bitwise::to_int32`, и inline-путь с ними
  совпадает.

## Fixed-work instructions (весь cohort)

CLI `0c932880c77b990a3447ed5c14ae72244783b7aebe11179dfb7e00e7dd1cd6db`.

| Бенч | Otter/Node | Δ к 0cc027bb9 |
|---|---:|---:|
| richards | 2.364637x | +0.134% |
| deltablue | 5.010699x | +0.002% |
| crypto | 1.598167x | +0.081% |
| raytrace | 2.536309x | +0.011% |
| earley-boyer | 1.584187x | +0.015% |
| regexp | 6.588679x | +0.013% |
| splay | 2.104793x | +0.640% |
| navier-stokes | 0.957982x | +0.011% |
| box2d | 2.655616x | +0.025% |
| gbemu | 2.651317x | **−16.867%** |
| mandreel | 1.355846x | +0.024% |
| zlib | 0.983958x | +0.042% |
| ts | 1.775158x | +0.021% |

Геометрическое среднее **2.142707518862→2.114129283508, −1.333744%**.
Регрессии сверх разброса: нет. splay и richards перемерены по 3 раза:
splay 9.185–9.235G (родитель 9.195G), richards 27.293–27.297G (родитель
27.298G) — шум.

## Wall-clock (медиана 5 свежих процессов, warmup 1)

| Бенч | otter ms | node ms | bun ms | o/node | RSS otter/node/bun MB |
|---|---:|---:|---:|---:|---|
| gbemu | 1433 | 465 | fail | 3.08 | 246/120/24 |
| mandreel | 1216 | 686 | 490 | 1.77 | 210/132/150 |
| zlib | 1917 | 1603 | fail | 1.20 | 123/83/10 |
| navier-stokes | 900 | 704 | fail | 1.28 | 33/57/24 |

Полная матрица 13 фикстур: геосреднее 2.52x node, 3.62x bun (9 фикстур).
Wall родителя не мерился (второй сборки HEAD нет); дельта среза — по
инструкциям.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-11e` и `full-2026-10-11g`, отчёт
`full-2026-10-12h`. Difftest host/x86 156/0 (новый
`typed_view_keyed_store.js`: все не-BigInt виды, int32 и double значения,
±2^63, NaN/±Infinity, clamped-округление, ключ вне длины, отрицательный и
дробный ключ, не-Number значения, BigInt view, detached буфер, resizable
буфер). VM 1245/0, JIT
270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13: вывод совпал.
Корпуса совпали с Node во всех режимах (interpreter, template, native,
stress 1/256).
