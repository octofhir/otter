# RegExp method entry: optimized code calls exec and test without a native frame

Parent `d027fccd6`. Machine provenance: [gate-summary.json](gate-summary.json),
[performance.json](performance.json), wall-clock `wall-final/`.

## Удалённый механизм

Вызов `re.exec(s)` / `re.test(s)` из оптимизированного кода шёл общим путём
native-вызова: трамплин call-entry (классификатор, резервация и публикация
host-фрейма), `host_call_entry` → `native_call` → `invoke_frame_native_call`
→ `NativeCallTarget::invoke` → тело `exec`. Затем внутри
`exec_once_native` полный спековый ладдер с handle-scope, чтением и
ToLength `lastIndex`, flatten субъекта — даже для промаха. exec-промах стоил
85 нс против 13 у node, а `Exec(re0)` (`/^ba/`, ~5.7 млн вызовов) — 484 мс
regexp-фикстуры против 97 у node.

Теперь:

- **Feedback.** Call-feedback узнаёт исходные `%RegExp.prototype.exec%` и
  `test` реалма: новые `IntrinsicCallKind::RegExpExec` / `RegExpTest`,
  корни в `RealmIntrinsics`.
- **Снимок.** На сайте `CallMethodValue` снимок запекает доказательство
  идентичности.
- **Граф.** После проверки идентичности метода (`CheckCellValue`) граф
  ставит узел `RegExpMethod`. Это committed-вызов reentrant-заглушки
  `STUB_JIT_REGEXP_EXEC` / `TEST` с полной семантикой метода: без
  native-фрейма и без трамплина, живые значения уходят в home-слоты.
- **Заглушка.** Сначала `exec_direct`: для RegExp-получателя с записываемым
  неотрицательным int32 `lastIndex` и строкового субъекта матч идёт без
  scope и без аллокаций. Промах и любой `test` завершаются сразу (запись
  `lastIndex`, legacy statics); попадание `exec` строит массив из уже
  найденной записи. Иначе — полное тело метода (coercions могут выполнить
  JS).

exec-промах 85 → **41 нс**, `re.exec`/`re.test` в микробенче 80 → 38 нс.

## Wall-clock (медиана 5; база — `store-proof-reuse/wall-final`)

| Бенч | otter до | после | o/node до | после | Δ otter |
|---|---:|---:|---:|---:|---:|
| richards | 1151 | 1149 | 1.81 | 1.81 | −0.2% |
| deltablue | 1171 | 1200 | 4.21 | 4.27 | +2.5% |
| crypto | 239 | 238 | 1.55 | 1.54 | −0.1% |
| raytrace | 1281 | 1220 | 2.38 | 2.27 | −4.7% |
| earley-boyer | 1628 | 1623 | 1.57 | 1.54 | −0.3% |
| regexp | 2205 | **2048** | 5.46 | **5.07** | −7.1% |
| splay | 703 | 707 | 2.34 | 2.37 | +0.6% |
| navier-stokes | 907 | 909 | 1.29 | 1.29 | +0.1% |
| box2d | 815 | 814 | 2.76 | 2.74 | −0.2% |
| gbemu | 1236 | 1232 | 2.62 | 2.65 | −0.3% |
| mandreel | 1198 | 1206 | 1.75 | 1.74 | +0.7% |
| zlib | 1913 | 1937 | 1.19 | 1.21 | +1.3% |
| ts | 3240 | 3293 | 2.73 | 2.81 | +1.6% |

regexp −7.1%. Сдвиги deltablue, raytrace и ts — шум: их инструкции не
изменились. Только время otter: ×0.995. Против усреднённых по двум прогонам
node и bun — 2.208 → **2.197** и 3.092 → **3.066**.

## Fixed-work instructions (диагностика; сумма по потокам)

regexp 32.99G → **30.38G (−7.92%)**; остальные −0.54…+0.13%. Геосреднее
1.950494 → **1.936432**.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-13k`, отчёт `full-2026-10-13l`. Difftest host/x86
166/0, VM 1256/0, JIT 276/0, x86 299/0, clippy host/x86. GC stress 256 всех 13
и `gcfull` (13 фикстур + корпуса `regexp_method_entry`, `background_compile`,
`parallel_scavenge`) — вывод совпал.

Новый корпус `regexp_method_entry` совпал с Node в interpreter, template,
native и стресс-режимах 256 и 1. Сценарии:

- горячие exec/test;
- global и sticky `lastIndex`;
- `lastIndex` с `valueOf` (полный путь);
- `lastIndex`, ставший незаписываемым, — TypeError;
- legacy statics;
- флаг `d`;
- числовой субъект и субъект-rope;
- `RegExp.prototype.exec`, подменённый посреди горячего цикла.

Unit-тесты: `exec_direct` (промах, test, global `lastIndex`, отказ для
неподходящих операндов), допуск method-форм, builder-сайт без
`CallJs`.
