# Constant Int32 fields: значение поля под существующей dependency

База: `58e69bae`. Доказательства: [gate-summary.json](gate-summary.json),
[performance.json](performance.json). Исходники заморожены на финальный gate
и все замеры; SHA CLI: `ba87a29cc55e517bd156627110128c9953b37bdea4a32155f3402e9f989dde03`.

## Удалённый механизм

Объект вроде `Direction` сохранял одно числовое значение поля, но Graph
заново читал его из storage. Теперь defining shape сохраняет первое Int32
значение в том же 64-битном summary. Если все receiver owners согласны,
загрузка становится `ConstInt32` под существующей `FieldType` dependency.
Дополнительных слов в объектах, header или shape нет.

- Бит 5 отмечает exact Int32 variant; bits 8–39 несут 32-битный payload.
  Shape-count и nullish bits там отсутствуют. Bake не разбирает этот payload
  как shape identities.
- Одинаковые наблюдения сохраняют константу. Первая другая запись навсегда
  заменяет её на category union; последующие одинаковые записи не возвращают
  constness. При любом widening меняется low byte, что сохраняет текущий
  контракт owner guards и install validation.
- Native birth и store probes обеих архитектур сравнивают точное значение
  до category membership. При collapse очищают числовой payload, включая
  переходы к null/undefined. Object membership никогда не читает constant
  payload как shape id. Watched widening обслуживает тот же noncollecting leaf.
- Store observation исключается только для доказанного равного Int32 RHS
  либо для membership в уже широком Int32 owner. Несколько receivers с
  разными константами используют обычную Int32 payload load.

## Доказательства

Runtime-регрессия прогревает `readDirection`, повторяет равную запись `1`,
затем меняет поле на `2`. Результат `[64000,60000]` совпадает с interpreter.
Точное native generation выходит один раз с `DependencyChanged`; его
owned IR содержит `ConstInt32(1)` и `CheckDependencies`, без `LoadOwnField`.
Replacement читает Int32 payload и проходит отдельный native entry probe
без новой компиляции, deopt, OSR или Template/interpreter entry.

VM-тест проверяет одинаковые записи, firing при другой цифре того же тега,
необратимость constness, последующее Float64 widening и освобождение watchers.
Packed-word тесты проверяют min/max, canonical variants, shape unions и
low-byte invariant для 61 состояния. Один исполняемый scalar-probe matrix
используется для обоих native encoders и проверяет dynamic/captured histories.

Corpus покрывает object-literal constants, одинаковые значения разных
объектов, native births/stores, Reflect/descriptor writes, nullish/object
widening, suffix bank, min/max и отрицательный ноль. Constructor-born
объекты с более широкими summary продолжают использовать их обычные bounds.

## Wall-clock

Медианы пяти свежих процессов, порядок движков ротируется по фикстуре.
База — `int32-field-loads/wall-final`, итог — `constant-int32-fields/wall-final`.
Перед замерами активных Otter-процессов нет.

| Фикстура | Otter, мс: Int32 → const | Δ wall | o/node | o/bun |
|---|---:|---:|---:|---:|
| richards | 989.6 → 957.8 | -3.21% | 1.503 | 1.771 |
| deltablue | 1169.8 → 1088.6 | -6.94% | 3.767 | 2.698 |
| crypto | 247.1 → 241.0 | -2.46% | 1.542 | — |
| raytrace | 1267.3 → 1269.6 | +0.18% | 2.376 | 4.379 |
| earley-boyer | 1704.6 → 1651.5 | -3.12% | 1.587 | 1.478 |
| regexp | 1686.0 → 1590.1 | -5.69% | 3.915 | 4.565 |
| splay | 812.9 → 761.6 | -6.30% | 2.397 | 4.106 |
| navier-stokes | 937.8 → 912.0 | -2.76% | 1.286 | — |
| box2d | 893.9 → 845.3 | -5.44% | 2.798 | 3.116 |
| gbemu | 1266.5 → 1226.7 | -3.15% | 2.593 | — |
| mandreel | 1263.1 → 1211.7 | -4.07% | 1.761 | 2.453 |
| zlib | 1999.3 → 1927.9 | -3.57% | 1.193 | — |
| ts | 3649.7 → 3366.4 | -7.76% | 2.881 | 3.282 |

Геосреднее o/node по 13 фикстурам: **2.123 → 2.120**.
Геосреднее o/bun по девяти общим успешным фикстурам: **2.918 → 2.901**.
Геосредняя дельта абсолютного времени Otter: **-4.20%**.
Bun завершил crypto, navier-stokes, gbemu и zlib с ошибкой.

Основной выигрыш deltablue повторился: финальная полная матрица дала −6,94%,
предварительное соседнее сравнение пяти запусков с сохранённым бинарём
Int32 — −6,14%. Предварительный CLI собирался отдельно, поэтому его замер
не подменяет финальные цифры. Отношения к Node изменились слабее абсолютных
времён; весь абсолютный выигрыш матрицы нельзя приписать только этому слайсу.

## Fixed-work инструкции

| Фикстура | Инструкции: Int32 → const | Δ |
|---|---:|---:|
| richards | 18.701 → 18.698G | -0.02% |
| deltablue | 20.505 → 20.195G | -1.51% |
| crypto | 4.578 → 4.567G | -0.23% |
| raytrace | 18.328 → 18.284G | -0.24% |
| earley-boyer | 29.536 → 29.478G | -0.20% |
| regexp | 23.110 → 22.974G | -0.59% |
| splay | 10.039 → 9.992G | -0.47% |
| navier-stokes | 7.922 → 7.881G | -0.51% |
| box2d | 12.968 → 12.913G | -0.43% |
| gbemu | 18.543 → 18.521G | -0.12% |
| mandreel | 19.223 → 19.180G | -0.22% |
| zlib | 24.338 → 24.325G | -0.05% |
| ts | 36.603 → 36.574G | -0.08% |

Геосреднее o/node: **1.8586 → 1.8519**.

## Gate

- VM release: **1262/1262**.
- JIT release: **285/285 arm64**, **308/308 x86**.
- Свойства и retraining: **15/15** на обеих архитектурах.
- Regex: **33 unit + 47 интеграционных**, обе архитектуры.
- Clippy release all-targets с `-D warnings`: host и x86.
- Difftest: **170/170** на host и x86, 0 расхождений.
- Test262 `full-2026-10-14c.json`: **53575 total / 52463 pass / 12 fail /
  1100 skip**, 0 crash/timeout/OOM. Coverage и failure set совпали с `14b`
  и `11e`; SHA runner записан сразу после прогона.
- Oracle: **48 запусков**, включая stress 1 и 256, совпадение с Node.
- GC: **26 запусков** по 13 fixed-work фикстурам, stress 256 совпал с normal.
- Full GC: **42 запуска**, включая новый constant-field corpus.
- Во всех финальных проверках и замерах source freeze и SHA CLI неизменны.
