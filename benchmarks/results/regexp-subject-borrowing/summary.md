# RegExp replace без копий subject и template

Строковая замена: **-15.47% … -12.79% wall**, инструкции **-15.66%** в узком сценарии. Callback replacement: **-13.31% … -12.27% wall**, инструкции **-12.37%** в узком сценарии. Три серии по пять samples на бинарь; это результат replacement-сценария, не всей матрицы.

Полная матрица: наблюдаемое геосреднее времени Otter **+2.86%**; FW **+0.011%**. Контроль Node/Bun изменялся от **-6.78% до +9.27%**. Общий wall-выигрыш не установлен, широкие wall-дельты помечены `scoreable: false`.

## Механизм

База: `c20d58d9`. Удалён `OwnedUnits`: строковая замена строит результат из двух краткоживущих code-unit views, не копируя целиком subject и template в отдельные Rust-векторы. В callback-пути копируются только нужные prefix/suffix spans; после каждого collecting callback subject перечитывается из handle. Owned output buffer живёт через callback, borrowed string view — нет.

Match ranges по-прежнему собраны до первого callback. Сохраняются порядок ToString, legacy statics, lastIndex, original subject и identity бросаемого значения. Финальная VM-строка выделяется после завершения всех borrows. Результат собирается в Latin-1, пока реальные units не требуют UTF-16; lone surrogates не проходят через UTF-8. Новых флагов, версий форматов и совместимых читателей нет.

Поведение перенесено из большого `regexp_fast.rs` в модуль `regexp_fast/replace.rs`; `UnitSink` и GetSubstitution имеют одного владельца. Пять unit-тестов проверяют numeric/named/optional/duplicate captures, prefix/suffix, exact UTF-16 units, narrowing и zero-width Unicode advancement. Новый корпус проверяет fresh ropes, перемещение roots во время callbacks и return-ToString, reentry, изменение exec/lastIndex и точные исключения.

## Wall-clock

Две свежие полные матрицы parent/current, median из пяти процессов. Перед замерами нет активных Otter-процессов; source/binary SHA не менялись. Node `v24.16.0`, Bun `1.4.3-canary.1+bf80d21c6`. У обоих бинарей одинаковые 13 Node-фикстур и 9 успешных Bun-фикстур; прежние Bun-отказы crypto, navier-stokes, gbemu и zlib не оценивались, stderr сохранён.

| Фикстура | Otter: parent → current, мс | Δ wall | o/node | o/bun |
|---|---:|---:|---:|---:|
| richards | 963.3 → 957.7 | -0.58% | 1.497 | 1.607 |
| deltablue | 1085.3 → 1044.6 | -3.75% | 3.678 | 2.568 |
| crypto | 249.1 → 235.2 | -5.58% | 1.519 | — |
| raytrace | 1283.9 → 1269.6 | -1.12% | 2.295 | 4.270 |
| earley-boyer | 1660.8 → 1686.3 | +1.54% | 1.581 | 1.496 |
| regexp | 1722.5 → 1802.7 | +4.66% | 4.254 | 4.976 |
| splay | 714.6 → 748.4 | +4.73% | 2.330 | 3.824 |
| navier-stokes | 911.8 → 922.5 | +1.17% | 1.283 | — |
| box2d | 839.2 → 876.4 | +4.43% | 2.866 | 3.009 |
| gbemu | 1220.1 → 1253.1 | +2.71% | 2.612 | — |
| mandreel | 1220.7 → 1239.4 | +1.53% | 1.795 | 2.456 |
| zlib | 1915.7 → 2418.5 | +26.25% | 1.472 | — |
| ts | 3309.8 → 3454.5 | +4.37% | 2.879 | 3.304 |

Наблюдаемое геосреднее o/node: **2.1107 → 2.1575**; o/bun по 9 фикстурам: **2.8786 → 2.8461**. Все наблюдаемые замедления сохранены. Качество сравнения и контрольные времена каждого эталона зафиксированы в JSON.

## Перепроверка regexp-фикстуры

В широкой матрице целевая regexp-фикстура показала +4,66%. Три отдельные серии по пять fresh processes на бинарь с чередованием parent/current дали следующий результат:

| Серия | Parent → current, мс | Δ wall |
|---|---:|---:|
| 1 | 1657.7 → 1643.5 | -0.86% |
| 2 | 1656.1 → 1652.8 | -0.20% |
| 3 | 1788.9 → 1814.1 | +1.41% |

Первоначальная регрессия не воспроизвелась. Устойчивый общий wall-выигрыш на этой фикстуре также не установлен; FW regexp сократился на 0,64%. Все 30 samples перепроверки сохранены в `performance.json`.

## Узкий replacement wall-пруф

Subject: 512 KiB Latin-1 плюс один `a`, один match `/a/g`, 1000 замен после 10 warmup. Replacement — строка `x` или JS-функция, возвращающая `x`. Проверяются длина результата и charCodeAt в позиции замены; checksum совпадает с Node. Normal production policy, три серии с чередованием порядка parent/current, по пять samples на бинарь. Исходники и все 60 samples включены в `performance.json`.

| Вариант / серия | Parent → current, мс | Δ wall |
|---|---:|---:|
| template 1 | 115.04 → 97.25 | -15.47% |
| template 2 | 113.26 → 96.72 | -14.60% |
| template 3 | 111.85 → 97.54 | -12.79% |
| functional 1 | 115.84 → 100.63 | -13.13% |
| functional 2 | 110.94 → 97.33 | -12.27% |
| functional 3 | 111.36 → 96.53 | -13.31% |

template: инструкции **0.896G → 0.756G (-15.66%)**.

functional: инструкции **0.863G → 0.757G (-12.37%)**.

## Fixed-work: диагностика

| Фикстура | Инструкции: parent → current | Δ |
|---|---:|---:|
| richards | 18.278 → 18.287G | +0.05% |
| deltablue | 19.800 → 19.816G | +0.08% |
| crypto | 4.559 → 4.562G | +0.08% |
| raytrace | 17.851 → 17.861G | +0.06% |
| earley-boyer | 29.488 → 29.497G | +0.03% |
| regexp | 22.998 → 22.851G | -0.64% |
| splay | 10.045 → 10.035G | -0.10% |
| navier-stokes | 7.877 → 7.893G | +0.20% |
| box2d | 12.959 → 12.965G | +0.05% |
| gbemu | 18.573 → 18.591G | +0.10% |
| mandreel | 19.192 → 19.225G | +0.17% |
| zlib | 24.326 → 24.338G | +0.05% |
| ts | 36.074 → 36.082G | +0.02% |

## Gate

- VM release **1269/1269**, JIT **292 ARM64 / 315 x86_64**.
- Runtime properties **16**, reclamation **6**, native context/body-kind proofs **4**, обе архитектуры.
- Regex **33 unit + 47 интеграционных**, обе архитектуры; Clippy all-targets release `-D warnings` на обеих.
- Difftest **173/173** на ARM64 и x86_64.
- Test262 `full-2026-10-14h.json`: **53575 total / 52463 pass / 12 fail / 1100 skip**, **0 crash/timeout/OOM** после валидации. Canonical coverage и failure set совпадают с `14g` и `11e`; runner SHA записан сразу после полного прогона.
- Oracle **72**, GC **26**, full-GC **48**: все сравнения прошли.
- Checked CLI с verify, strides **1–16**: **8304/8304**, все **173** корпуса в interpreter/template/production; дополнительно replacement regression **48/48**. Journal свежий, source и binary SHA неизменны.
- Новый корпус совпадает с Node в шести release-режимах (3 tiers × normal/stress1); на родительском checked CLI stress1+verify также совпал с Node.
- Source freeze **5** файлов, hashes CLI/runner/checked, logs и измерений сохранены в `gate-summary.json` / `performance.json`.
