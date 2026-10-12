# Intrinsic method receivers: string and RegExp method lookups in generated code

Parent `8cac0b914`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`, profiles
`sample-regexp*.txt`.

## Удалённые механизмы

- Template `CallMethodValue` на примитивной строке или RegExp: после промаха
  IC-пробы каждый вызов шёл в `STUB_JIT_RESOLVE_METHOD` (полный runtime-поиск
  метода), хотя статическая CacheIR-программа прототипа уже лежала в снимке.
  Теперь шаблон исполняет программу (тег тела, guard, слот прототипа) до
  стаба; стаб остаётся только для получателей, чей вызов записывает
  call-фидбэк для оптимизатора (замыкания, коллекции).
- RegExp-получатель не имел программы ни в одном тире: тело не несло
  машинно-читаемого признака «нет expando, нет переопределённого
  `[[Prototype]]`», а `is_generated_receiver` отвергал тег RegExp. Теперь
  тело хранит слово `jit_guard_flags` (expando / proto override), программа
  проверяет его и активный realm (RegExp без override берёт прототип
  активного realm), и обе ветки — шаблон и оптимизатор — читают
  `%RegExp.prototype%.exec/test/…` напрямую. В Exec из regexp `re.exec`
  в оптимизированном коде стал `LoadNamedProperty` + нативный вызов вместо
  общего `LoadPropertyCached`-стаба.

Профиль regexp до: `jit_resolve_method_stub` 13.6% главного потока;
после: ≥300 тыс. разрешений → 5.4 тыс.

## Wall-clock (медиана 5; база — `old-generation/wall-final`)

| Бенч | otter до | после | o/node до | после | RSS MB |
|---|---:|---:|---:|---:|---:|
| richards | 1368 | 1309 | 2.12 | 2.06 | 52 |
| deltablue | 1508 | 1462 | 5.25 | 5.13 | 106 |
| crypto | 244 | 238 | 1.54 | 1.55 | 46 |
| raytrace | 1515 | 1484 | 2.80 | 2.79 | 104 |
| earley-boyer | 1611 | 1608 | 1.56 | 1.56 | 113 |
| regexp | 3297 | **3006** | 8.17 | **7.48** | 187 |
| splay | 701 | 695 | 2.41 | 2.14 | 575 |
| navier-stokes | 906 | 908 | 1.28 | 1.17 | 34 |
| box2d | 833 | 851 | 2.80 | 2.87 | 157 |
| gbemu | 1442 | 1447 | 3.08 | 3.11 | 255 |
| mandreel | 1193 | 1200 | 1.75 | 1.76 | 233 |
| zlib | 1916 | 1914 | 1.19 | 1.19 | 124 |
| ts | 3267 | 3273 | 2.87 | 2.89 | 545 |

regexp −8.8%. Геосреднее к node 2.42 → **2.36**, к bun 3.47 → **3.38**
(bun падает на crypto/navier-stokes/gbemu/zlib, как и в базе — геосреднее
по 9). Остальные фикстуры — в разбросе базы (richards/deltablue/raytrace
база была со звёздочкой, перемер 10 прогонами 2.05/5.22/2.77).

## Fixed-work instructions (диагностика; сумма по потокам)

regexp 48.23G → 45.03G (−6.62%). Геосреднее 2.144782 → 2.138142 (−0.31%);
остальные фикстуры −0.07…+0.77% (ts +0.77%, gbemu +0.69% — потоки фоновой
компиляции и GC-помощников).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-12l`, отчёт `full-2026-10-13a`. Difftest host/x86,
VM/JIT `--lib --release` host и x86, clippy host/x86 — зелёные. GC stress 256
всех 13 фикстур и `gcfull` — вывод совпал. Новый корпус
`intrinsic_method_receivers` (собственный `exec`, подкласс, `setPrototypeOf`,
переопределённый `String.prototype.replace`, удалённый
`RegExp.prototype.test` на горячих сайтах) совпал с Node в interpreter,
template, native и стресс-режимах 256 и 1.
