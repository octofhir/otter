# Resolved intrinsic calls

Parent — подписанный `a9b41e7436d6647b9622d8c146bcd1988def8e91`. Before CLI SHA-256 — `9b9f54616e092214218a0005d84357f7393b58e497b116d7fc025bdb9298dc08`. Final host CLI pinned SHA — `f5efe0120122a19c425defddf6a3a221d3970065b77e12dc48566d9a90962140`. Correctness, последовательный final profile и повторный полный fixed-work cohort завершены. [Collector gate-summary](gate-summary.json) — **PASS**. Коммит создаётся отдельным GPG-вызовом после review гейта; подпись проверяется через `git log --show-signature -1`.

Final source freeze: **148 путей**, SHA `6d01ab0d5b43c6b998fe49ef03ce03344f7e3ecd5705ef4951008a8d0f377ead`. Host/x86 builds завершены; свежие units/runtime/clippy records ниже имеют exit 0 и тот же freeze digest. Raw correctness, profile и instruction records прочитаны; final collector сверяет их identities.

Обычные `Call`/`New` сначала получают настоящий callee и вычисляют аргументы. VM feedback хранит один scalar intrinsic kind; `native_calls` передаёт один generation-owned proof с адресом стабильной traced identity cell и source realm. Template ARM64/x86 и Graph используют одну таблицу допустимых форм: Array Call/New с 0–1 аргументом, Boolean Call, Number Call без аргументов, Object Call/New без аргументов. Guard загруженного callee предшествует эффектам. Eager miss восстанавливает исходный Call/New с уже вычисленными операндами. Неподдержанные формы сохраняют обычное Native linkage.

Удалены выбор семантики по написанию builtin, array-only proof map, заменённые opcode/dispatch/stub/variadic пути и их fixtures. Один compiler prepared-identifier owner удерживает выбранную среду и reference через RHS, iterator/getter/default и PutValue. Method fusion допускается только при доказанной перестановке аргументов с GetMethod. Cross-clause Switch TDZ использует существующий checked context slot; частные register locals сохраняют простой путь. Бюджет инлайнинга не увеличен.

**Финальный emitted proof, pinned CLI:** OrderedCollection перестал содержать Generic и LoadWindow; empty Array LAB hit не вызывает helper и не делает register-window round trip. Private entry сократился с 63 до 53 emitted инструкций, его frame span — с 43 до 33. Вместо 12 window setup/fill инструкций остались две. Actual accepted splices — 0→21, каждый заряжен 46 encoded-bytecode B при прежних thresholds. Корректный callee добавил LoadGlobalBinding (46 emitted инструкций) и identity guard (4); native body вырос с 2 376 до 2 584 B. Финальный [before/after comparison](after-comparison.json) подтвердил эти counts; [ранний build 4 proof](intrinsic-only-f505-evidence.md) имеет отдельную intermediate identity и сохраняется как history.

Source/root/reconstruction исправления используют существующих владельцев. NewError декодирует kind и выбирает prototype по innermost source function; rooted message и realm admission предшествуют coercion/allocation. Восстановленный parent сохраняет Call PC и resume flag, пока queued child не завершён. Canonical Array.prototype имеет enduring brand в существующей metadata объекта; snapshot, realm disposal и GC сохраняют category. Один NativeCtx classifier и ArraySetLength owner обслуживают VM, clone и worker; публичные payload остаются owned data. [Source error proof](new-error-realm-before.md), [restored child proof](restored-inline-source-fix.md), [prepared identifier/TDZ](compiler-tdz-proof.md).

Object toString и Array fallback используют один fallible `Interpreter::is_array` classifier и scoped driver. Удалён отдельный Proxy walker, который терял canonical Array brand и скрывал revoked exception. IsArray предшествует observable tag getter; actual receiver остаётся rooted через collecting `join`/tag Get, поздняя revocation сохраняет ранее выбранный builtin tag. Primitive ToObject читает typed canonical realm prototypes; заменённые или null global String/Boolean/Number/Symbol/BigInt не выбирают wrapper. Object self-construction сверяет NewTarget с canonical callable root, поэтому сохранённый Object constructor сохраняет boxing semantics после изменения global binding. [Исходный brand/order proof](array-brand-spec-review.md); расширенный корпус 35 checks / 448 controlled allocations прошёл все восемь final oracle observations.

Receiver allocation accounting получает typed `ReceiverAllocationMiss` непосредственно из emitted `Kind::NewReceiver` на обеих архитектурах. VM сравнивает этот reason, без декодирования source `New`; Array length allocation того же opcode остаётся `AllocationMiss` и не увеличивает receiver counter. Это исправление диагностического ownership, без изменения allocation/completion behavior.

Ранние oracle evidence ограничены своими producers: intrinsic corpus на старом build 6 совпал во всех записанных режимах и stress strides (74 checks); Node подтвердил 63 Switch/reference checks, 15 method-order checks и restored completion outputs. Эти наблюдения не заменяют повторный гейт финального source freeze.

Исторические failed attempts сохранены. Worker12 имел 2 passed / 1 failed из-за проверки awaited script completion. Исправлен только test child: message payload сохраняется в realm-global marker, затем eval проверяет точный marker. Runtime API не обещает универсальный completion `undefined`; worker13 прошёл 3/0 без такого предположения.

Новые difftest-корпуса: `intrinsic_call_proofs.js`, `method_argument_order.js`, `restored_inline_completion.js`, `switch_tdz_method_order.js`, `array_brand_to_string.js`. Полный difftest — **145/0 host и 145/0 x86**, одинаковый seed `340717225298`, interpreter oracle. `oracle-final.json` — **PASS 40**: Node плюс семь Otter modes/strides на каждый из пяти корпусов, exit 0, одинаковые raw outputs, current corpus hashes и unchanged pinned CLI. Observation и fixed-work drivers требуют явный final binary SHA до первого запуска и проверяют его в конце.

Единственный full-e test262: **53 575 total, 52 463 passed, 12 failed, 1 100 skipped**; timeout/OOM/crash — 0. Validator подтвердил тот же fail set, что baseline b и d MD; timeout rechecks не потребовались. Final runner SHA `60e9db994e23a99358ea688235da5bb00346bf0ce818452137beba02437d3be8` совпадает у completed report, validator и actual executable. Combined-build runner `661c6ffc...` сохранён отдельно как history. `gc-final.json` — **PASS 26**: normal/stress 256 на всех 13 бенчах, exit 0 и одинаковые outputs, unchanged pinned CLI.

| Финальная проверка | Статус |
|---|---|
| Host build13 / x86 build5; final host CLI SHA | Raw PASS / pinned |
| Final oracle, пять корпусов × восемь observations | PASS 40 |
| 3 pure + 1 artifact Deltablue на final CLI | Complete / comparison PASS, identities unchanged |
| Fixed-work all 13; exact deltas и все регрессии | Final repeat PASS; geo −0.352930%, шесть положительных delta ниже |
| Full test262: прежние 12 fail, timeout 7 200 000 ms | Validator PASS; timeout/OOM/crash 0 |
| Host/x86 difftest, timeout 120 000 ms | 145/0 на каждом, seed 340717225298 |
| GC stress 256 на всех 13 бенчах | PASS 26, outputs совпадают |
| Host clippy10 / x86 clippy5 | Raw PASS |
| VM host / JIT host / JIT x86 lib | 1236/0 / 260/0 / 282/0 |
| Runtime Array entry / empty allocation bins; clone / worker filters | 9/0 / 10/0; 9/0 / 3/0 |
| Final collector gate-summary | PASS / current freeze148 |
| Создание коммита | Отдельный GPG-вызов после review гейта |

## Итоговое fixed-work измерение

Повторный **полный cohort всех 13 бенчей**, завершённый в 13:22:27 UTC на unchanged pinned CLI, принят итоговым. Ratio Otter/Node означает retired instructions, меньше лучше. Геометрическое среднее **2.392569906614916x → 2.3841258185276977x**, delta **−0.3529296286755246%**. Deltablue −3.0282824147774168%, Mandreel −0.8181547160020064%; Box2D +0.9220713686664217%. Все шесть положительных delta показаны без округления до unchanged: richards, earley-boyer, regexp, splay, navier-stokes, box2d. Это complete cohort, без подстановки отдельных Mandreel повторов.

| Бенч | Otter/Node | Δ Otter к parent |
|---|---:|---:|
| richards | 2.46690x | +0.070583% |
| deltablue | 6.92080x | -3.028282% |
| crypto | 1.60015x | -0.260524% |
| raytrace | 4.13874x | -0.039252% |
| earley-boyer | 1.58857x | +0.053765% |
| regexp | 6.99625x | +0.009218% |
| splay | 2.77850x | +0.042920% |
| navier-stokes | 1.09805x | +0.005371% |
| box2d | 2.70185x | +0.922071% |
| gbemu | 3.19635x | -0.189386% |
| mandreel | 1.37750x | -0.818155% |
| zlib | 0.98414x | -0.612386% |
| ts | 1.79134x | -0.692883% |

Initial producers: [first-performance.json](first-performance.json), [first-fixed-work-identity.json](first-fixed-work-identity.json), `first-fw-raw/`, `first-fixed-work.stdout` и `first-fixed-work.stderr`. Итоговые producers: [performance.json](performance.json), [fixed-work-identity.json](fixed-work-identity.json), `fw-raw/`. Exact comparison обоих cohorts использует `../object-protocol-operation/performance.json` и тот же fixed Node denominator. Каждый cohort самостоятельный, mixed-cohort геометрия не используется.

## Первоначальный cohort и Mandreel выброс

Первоначальный полный cohort дал geo 2.4130132918495186x (+0.8544529954205959%) и Mandreel 21 576 131 904 (+12.385531088657986%). Его семь положительных delta сохраняются как фактическое initial observation. **Причина Mandreel выброса неизвестна**; тот же frozen binary без production edits дал три отдельных отрицательных delta и затем −0.8181547160020064% в повторном complete cohort. Стабильная +12.385531% регрессия не воспроизвелась; compiler/Graph гипотезам эта причина не приписывается.

| Бенч первоначального cohort | Otter/Node | Δ Otter к parent |
|---|---:|---:|
| richards | 2.46451x | -0.026474% |
| deltablue | 6.92202x | -3.011097% |
| crypto | 1.60959x | +0.328384% |
| raytrace | 4.14032x | -0.000963% |
| earley-boyer | 1.59382x | +0.384486% |
| regexp | 6.97593x | -0.281191% |
| splay | 2.77712x | -0.006770% |
| navier-stokes | 1.09951x | +0.138206% |
| box2d | 2.72882x | +1.929399% |
| gbemu | 3.20620x | +0.118260% |
| mandreel | 1.56088x | +12.385531% |
| zlib | 0.98500x | -0.525750% |
| ts | 1.81116x | +0.406401% |

Три отдельных Mandreel повтора: 19 122 174 530 (−0.396616516529%), 19 037 252 608 (−0.838956939155%), 19 032 125 174 (−0.865664663958%), exit0. [Raw repeats](mandreel-fw-repeats.json). Redundant identifier `resolve_ref` остаётся отдельной compiler оптимизацией следующего coherent slice, без causal claim для выброса.

## Финальный профиль

Финальный Deltablue capture выполнен последовательно: три pure runtime запуска и отдельный один artifact launch; обе capture identities имеют pinned SHA до/после, exit 0 и `binaryUnchanged: true`. [Final comparison](after-comparison.json) проверяет unique `OrderedCollection` name/module, затем его process-local ID (в обоих captures оказался 505), полные self listings и собственные perf maps каждого процесса. Pure self totals 6 969→6 347; physical subject generated 124→0, native 132→1. Artifact total self 2 449→2 242, generated 1 642→1 511; outside/missing 0. Это sample observations, не retired deltas.

| Artifact generated self span | Before | Final |
|---|---:|---:|
| Body / IR | 1 375 | 1 278 |
| Private entry | 202 | 162 |
| Ordinary return | 38 | 46 |
| Cold | 14 | 22 |
| Constructor completion | 10 | 2 |
| Entry cold construct | 3 | 1 |

Physical subject artifact self 47→0; subject-owned instruction regions, включая actual splices в caller, 25→28. Source instruction-region native self 40→3. В отсутствие labelled source region cold owner не угадывается; inclusive samples и cross-process artifact addresses в totals не входят. Финальное emitted тело c44 подтверждает Generic/LoadWindow 0, LAB fit `[980,1076)` без BLR, cold `[1076,1156)` с `jit_new_array` BLR на +0x45c. Source/root/completion records сохраняются для холодной collecting границы. Accepted splices 0→21, 9 unique sites, 12 code objects, depth1=12/depth2=9, charged cost46B каждый, candidate41→41; event truncation/drops0. Native entry63→53/frame43→33 и added LoadGlobalBinding46/identity4 совпадают с ранним independent emitted observation.
