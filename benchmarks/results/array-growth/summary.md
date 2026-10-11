# Array growth in generated code

Parent `7fb83de238063a7e9058edd9945c3627415f8041`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

После inline push/pop рост массива (полный буфер или первый push в массив
без slab) шёл в committed builtin: `RuntimeCall`, handle scope, SmallVec
аргументов, `array_push_scoped`, `push_with_roots`,
`reserve_dense_capacity` (allocator + копия через `get`/`set` с проверкой
kind) — 14% сэмплов deltablue. Теперь рост — часть узла `ArrayPush`.

## Архитектура

- `emit_grow_tagged_slab` (обе архитектуры, в модуле nursery allocation):
  проверка ёмкости против large-object порога, bump в nursery LAB, header
  с динамическим размером, capacity/len/kind/dirty interval, копия живого
  префикса, публикация курсора последней.
- `ArrayPush`: полный буфер или отсутствующий slab (kind Empty, длина 0) →
  ёмкость `max(4, 2·cap)` как у VM, новый slab ставится в shell (handle,
  base, cap, kind, длины); barrier только Array → slab. При marking или
  промахе LAB — прежний committed builtin.
- Скопированные слова и новое значение лежат в молодом slab вне marking:
  barrier им не нужен; dirty interval покрывает префикс и следующий слот.
- `JitArrayStorage`: handle slab, header word молодого slab, предел ёмкости.

## Fixed-work (весь cohort)

CLI `9deae26ebaa53954310dc07a3f3ecb27ddf0bf2222f445dae306c676eb53505a`.

| Бенч | Otter/Node | Δ к 7fb83de23 |
|---|---:|---:|
| richards | 2.460763x | +0.016163% |
| deltablue | 5.325313x | **-12.917699%** |
| crypto | 1.595114x | +0.006566% |
| raytrace | 4.137996x | -0.009944% |
| earley-boyer | 1.584494x | +0.000545% |
| regexp | 6.602608x | +0.001321% |
| splay | 2.102959x | +0.116524% |
| navier-stokes | 1.098583x | +0.058068% |
| box2d | 2.705881x | +0.011159% |
| gbemu | 3.174806x | -0.003211% |
| mandreel | 1.375660x | +0.008734% |
| zlib | 0.983635x | +0.008868% |
| ts | 1.792717x | -0.050413% |

Геометрическое среднее **2.298389057858→2.274351833994, −1.045829%**.
Регрессии: splay +0.117%, navier-stokes +0.058%, richards +0.016%, box2d
+0.011%, zlib +0.009%, mandreel +0.009%, crypto +0.007%, regexp +0.001%,
earley-boyer +0.001% (в пределах шума одиночного прогона ±0.5%).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-12a`. Difftest host/x86 151/0. VM 1245/0,
JIT 270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13: вывод совпал.
Корпуса `array_push_pop_inline` и `allocation_site_pretenuring` совпали с
Node во всех режимах.
