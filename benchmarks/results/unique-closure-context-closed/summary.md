# Собственный closure-контекст под dependency

Wall-clock относительно `11fcbf54a`: **Richards −1,28%, DeltaBlue −4,77%, Raytrace −2,98%, TypeScript −3,44%**. Геосреднее абсолютного времени Otter по 13 фикстурам: **−1,46%**. FW сокращает инструкции Richards на **2,23%**, DeltaBlue на **2,00%**, Raytrace на **2,28%**; общий o/node: **1,8519 → 1,8413**.

## Механизм и корректность

База: `11fcbf54a` (`constant-int32-fields`). Собственные Graph-тела и обычные body-inlines теперь читают watched context slots как константы, когда все созданные замыкания источника используют один контекст. У вызовов нет новой проверки identity: действует общий `CheckDependencies`.

Факт живёт в VM-owned registry. Его первое значение переписывается moving GC; повторное создание с тем же контекстом ничего не меняет. Другой контекст снимает специализацию навсегда, освобождает первый root и сигнализирует подписанным поколениям. Обе native allocation реализации проверяют контекст до LAB effects: первый/другой контекст проходит обычный allocation miss с одним VM-owned созданием. В коде, подготовленном после снятия факта, нет этой birth-проверки.

Для reclamation кандидатные источники освобождают optimization roots до GC-census. Живой eval-closure сохраняет источник; после удаления последней JS-ссылки чанк и его payload освобождаются. `this` и `new.target` остаются отдельными фактическими bindings каждой closure. Внутренние форматы изменены в месте использования, без совместимых читателей и новых флагов.

Runtime-пруф: native sibling births сохраняют `ConstInt32(7)`, следующий контекст вызывает ровно один `DependencyChanged`, replacement читает живой context; обе функции проходят отдельные native entry probes. Native lexical allocation proof сохраняет fit, collecting miss, точные inline source/realm и неизменные измеряемые поколения; warmup сначала принимает allocation-site решения. Проверки ledger различают точный chunk payload и дополнительно освобождаемые text/index leases.

Checked-GC дополнительно обнаружил stale receiver после collecting flatten в `charCodeAt`. Плоские строки сохраняют путь без scope; collecting rope-путь держит receiver в handle scope и читает обновлённый handle. `includes` сохраняет полные IsRegExp/ToString и их исключения, держит receiver, search и position в handles; два contiguous string operands сканируются без scope. Корпус проверяет свежие ropes в прямых и `.call()` вызовах, однократный ToString, порядок @@match/ToString, RegExp с @@match=false и identity бросаемого значения.

## Wall-clock

Медиана пяти свежих процессов, без активных Otter-процессов перед замером. База и текущая матрица используют одинаковые 13 Node-фикстур и одинаковое множество 9 успешных Bun-фикстур. Node `v24.16.0`; Bun `1.4.3-canary.1+bf80d21c6`. У Bun прежние отказы на crypto, navier-stokes, gbemu и zlib сохранены в stderr и исключены из таймингов.

| Фикстура | Otter: база → слайс, мс | Δ wall | o/node | o/bun |
|---|---:|---:|---:|---:|
| richards | 957.8 → 945.5 | -1.28% | 1.478 | 1.764 |
| deltablue | 1088.6 → 1036.6 | -4.77% | 3.616 | 2.584 |
| crypto | 241.0 → 235.9 | -2.14% | 1.533 | — |
| raytrace | 1269.6 → 1231.7 | -2.98% | 2.315 | 4.292 |
| earley-boyer | 1651.5 → 1669.9 | +1.11% | 1.605 | 1.570 |
| regexp | 1590.1 → 1599.8 | +0.61% | 3.943 | 4.650 |
| splay | 761.6 → 729.2 | -4.25% | 2.357 | 4.014 |
| navier-stokes | 912.0 → 912.7 | +0.08% | 1.292 | — |
| box2d | 845.3 → 839.0 | -0.75% | 2.830 | 3.037 |
| gbemu | 1226.7 → 1222.8 | -0.31% | 2.599 | — |
| mandreel | 1211.7 → 1207.4 | -0.36% | 1.758 | 2.456 |
| zlib | 1927.9 → 1923.6 | -0.22% | 1.193 | — |
| ts | 3366.4 → 3250.5 | -3.44% | 2.842 | 3.344 |

Геосреднее o/node: **2,1199 → 2,1060** по 13 фикстурам. o/bun: **2,9007 → 2,8954** по 9. Earley wall **+1,11%** при практически неизменном FW; regexp wall **+0,61%**, FW **+0,04%**. Эти наблюдения включены в общую матрицу.

## Fixed-work: диагностика

| Фикстура | Инструкции: база → слайс | Δ |
|---|---:|---:|
| richards | 18.698 → 18.282G | -2.23% |
| deltablue | 20.195 → 19.790G | -2.00% |
| crypto | 4.567 → 4.552G | -0.34% |
| raytrace | 18.284 → 17.868G | -2.28% |
| earley-boyer | 29.478 → 29.477G | -0.00% |
| regexp | 22.974 → 22.984G | +0.04% |
| splay | 9.992 → 10.005G | +0.13% |
| navier-stokes | 7.881 → 7.882G | +0.01% |
| box2d | 12.913 → 12.956G | +0.33% |
| gbemu | 18.521 → 18.573G | +0.28% |
| mandreel | 19.180 → 19.152G | -0.15% |
| zlib | 24.325 → 24.324G | -0.00% |
| ts | 36.574 → 36.125G | -1.23% |

## Gate

- VM release: **1264/1264**.
- JIT release: **287/287 ARM64**, **310/310 x86_64**.
- Runtime properties: **16/16**, code eviction **6/6**, native context/closure allocation **2/2** на обеих архитектурах.
- Regex: **33 unit + 47 интеграционных**, обе архитектуры.
- Clippy release all-targets `-D warnings`: обе архитектуры.
- Difftest: **171/171** на ARM64 и x86_64.
- Test262 `full-2026-10-14f.json`: **53575 total / 52463 pass / 12 fail / 1100 skip**; **0 crash/timeout/OOM**. Каноническое coverage и failure set совпали с `14c` и `11e`; runner SHA записан сразу после полного прогона.
- Oracle **56**, GC **26**, full-GC **44**: все сравнения прошли.
- Checked binary, `OTTER_GC_VERIFY=1`, strides **1–16**: **8208/8208** запусков, все **171** корпуса в interpreter/template/production; source freeze и binary SHA неизменны, journal не переиспользован после изменения бинаря.
- Дополнительный rope/coercion regression **48/48** и точное совпадение с Node при stress1 во всех трёх checked-режимах.
- Свежие native artifacts: **13** relocations `closureContextCell`, stdout совпал с Node.
- SHA CLI и source freeze неизменны во всех closing-проверках и замерах; подробности в `gate-summary.json` и `performance.json`.
