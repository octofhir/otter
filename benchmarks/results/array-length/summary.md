# Array length from feedback; loop-invariant motion past an OSR entry

Parent `743c357b7`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

`.length` всегда шёл через `LoadPropertyCached { length: true }`: inline-
ветки для массива, строки и typed array плюс committed runtime-путь. Узел
`may_collect`, поэтому каждое чтение длины стирало все факты о куче посреди
итерации: в deltablue `size()` в условии цикла заставлял заново проверять
поля, стабильные формы и методы, а результат каждый раз конвертировался
из tagged в int32.

## Архитектура

- IC-слот named load получил бит `length` прочитан у не-Array; его ставят
  интерпретатор и runtime-путь генерированного кода, первая установка
  двигает epoch feedback.
- Снапшот: `length_reads_arrays_only` для `length`-сайта, который запускался
  и видел только массивы.
- `LoadArrayLength`: проверка Array-тега и чтение `length` как int32, eager
  deopt для не-массива или длины вне int32. Узел не стирает факты.
- LICM: ничего не выносится из блоков цикла, которые достижимы со входа в
  обход заголовка (OSR-вход во внутренний цикл): значение не существовало бы
  на этом пути. Найдено гейтом (attempt 1, `strict_equality_kinds.js` в
  eval-режиме): внутренний цикл без вызовов стал кандидатом LICM.

## Fixed-work (весь cohort)

CLI `87033bef0be498df12c4d9c9190879be0a5cccbc3442f1b0405a186bfba6c1c2`.

| Бенч | Otter/Node | Δ к 743c357b7 |
|---|---:|---:|
| richards | 2.361462x | -0.006023% |
| deltablue | 5.010609x | **-4.737801%** |
| crypto | 1.596879x | -0.031354% |
| raytrace | 2.536028x | -0.500759% |
| earley-boyer | 1.583954x | -0.107385% |
| regexp | 6.587791x | -0.235784% |
| splay | 2.091414x | +0.006743% |
| navier-stokes | 0.957877x | +0.011440% |
| box2d | 2.654944x | +0.019367% |
| gbemu | 3.189237x | +0.137815% |
| mandreel | 1.355517x | -0.007005% |
| zlib | 0.983548x | +0.007827% |
| ts | 1.774781x | -0.373970% |

Геометрическое среднее **2.152512492487→2.142707518862, −0.455513%**.
Регрессии: gbemu +0.138%, box2d +0.019%, navier-stokes +0.011%, zlib
+0.008%, splay +0.007% (шум одиночного прогона).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-12e`. Difftest host/x86 155/0 (новый
`array_length_feedback.js`: длина меняется между чтениями, сайт позже видит
строку/typed array/arguments/объект, длина вне int32, OSR во внутренний цикл
с глобальным массивом). VM 1245/0, JIT 270/0, x86 293/0, clippy host/x86.
GC stress 256 всех 13: вывод совпал. Корпуса совпали с Node во всех режимах.
