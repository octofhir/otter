# Store proof reuse: a store trusts the shape proof its receiver already has

Parent `5d6497acf`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`.

## Удалённый механизм

Повторный `CheckShapes { writable: true }` перед каждой записью в собственное
поле. Builder знал форму получателя — из загрузки, из сводки типа поля или из
стабильной формы, пережившей вызов. Но такое доказательство помечалось как
«не исключающее прототип», и запись проверяла форму ещё раз.

Формы записывающего сайта берутся из store-хэндлеров, а их не получает ни
одна форма прототипа (store IC отказывает прототипам). Значит, известная
форма, входящая в формы сайта, уже исключает прототип. Эмиттер `CheckShapes`
и так не различал writable и обычную проверку, повторялась ровно та же
проверка формы. Теперь `check_shapes` для такого доказательства лишь
помечает раскладку как writable.

## Wall-clock (медиана 5; база — `shape-dependencies/wall-final`)

| Бенч | otter до | после | o/node до | после | Δ otter |
|---|---:|---:|---:|---:|---:|
| richards | 1198 | **1151** | 1.89 | **1.81** | −3.9% |
| deltablue | 1227 | **1171** | 4.19 | 4.21 | −4.6% |
| crypto | 241 | 239 | 1.53 | 1.55 | −0.9% |
| raytrace | 1260 | 1281 | 2.33 | 2.38 | +1.7% |
| earley-boyer | 1609 | 1628 | 1.56 | 1.57 | +1.2% |
| regexp | 2212 | 2205 | 5.50 | 5.46 | −0.3% |
| splay | 731 | 703 | 2.37 | 2.34 | −3.8% |
| navier-stokes | 913 | 907 | 1.28 | 1.29 | −0.6% |
| box2d | 823 | 815 | 2.74 | 2.76 | −1.0% |
| gbemu | 1235 | 1236 | 2.60 | 2.62 | +0.1% |
| mandreel | 1210 | 1198 | 1.75 | 1.75 | −1.0% |
| zlib | 1921 | 1913 | 1.19 | 1.19 | −0.4% |
| ts | 3246 | 3240 | 2.73 | 2.73 | −0.2% |

У deltablue node в базовом прогоне был медленнее (293 → 278 мс), поэтому
o/node не изменилось при −4.6% otter. raytrace и earley — в разбросе: их
инструкции не изменились. Только время otter: ×0.989. Против усреднённых по
двум прогонам node и bun — 2.220 → **2.196** и 3.097 → **3.056**.

## Fixed-work instructions (диагностика; сумма по потокам)

richards 22.85G → **21.86G (−4.36%)**, deltablue −1.14%, box2d −1.07%, gbemu
−0.95%; остальные −0.19…+0.07%. Геосреднее 1.961634 → **1.950494**.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13j`, отчёт `full-2026-10-13k`. Difftest host/x86
165/0, VM 1254/0, JIT 274/0, x86 297/0, clippy host/x86. GC stress 256 всех 13
и `gcfull` (13 фикстур + корпуса `fresh_receiver_appends`,
`background_compile`, `parallel_scavenge`) — вывод совпал. Корпуса
`fresh_receiver_appends` (получатель становится прототипом посреди
конструктора) и `field_store_owner` совпали с Node в interpreter, template,
native и стресс-режимах 256 и 1. Builder-тест: запись после загрузки той же
формы не добавляет второй `CheckShapes`.
