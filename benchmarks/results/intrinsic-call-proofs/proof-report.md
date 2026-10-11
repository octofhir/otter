# Intrinsic-call contracts: proof

Parent `a9b41e7436d6647b9622d8c146bcd1988def8e91`. Correctness и final profile завершены; повторный complete all-13 cohort принят итоговым. [Collector](gate-summary.json) — **PASS**. Ниже раздельно сохранены baseline/final samples, final emitted code, instruction cohorts и source ownership. Только подписанный коммит и проверка G **PENDING**.

Final source inventory содержит 148 путей; SHA `6d01ab0d5b43c6b998fe49ef03ce03344f7e3ecd5705ef4951008a8d0f377ead`. Final host CLI pinned SHA `f5efe0120122a19c425defddf6a3a221d3970065b77e12dc48566d9a90962140`; correctness records завершены и прочитаны. Final 3 pure + 1 artifact profile, final complete instruction cohort и collector PASS имеют pinned CLI; initial cohort и Mandreel repeats сохраняются отдельно. Intermediate build 4 имеет отдельную identity и не считается producer этого freeze.

## Владение и переходы

`resolved callee + evaluated args → native_calls::Intrinsic(proof) → identity guard → specialized value/allocation` — один путь для двух архитектур и tiers. Scalar feedback не держит GC handle. Код запекает адрес generation-owned traced cell; GC переписывает её callable. Proof baking требует активный source realm, shared form admission проверяет realm и непустой identity cell. Guard miss сохраняет loaded callee и все evaluated args, включая дополнительные игнорируемые Boolean operands, и восстанавливает original Call/New без повторного lookup/coercion. Foreign-source body inlining и proof baking decline; обычная source-aware Native boundary остаётся владельцем completion.

Compiler prepared identifier — один carrier существующих operands: выбранные with receiver/flag, eval reference, static NameRef и strict-global existence proof. Prepare/read/store используется plain, compound, logical и destructuring assignment; lookup происходит до RHS или извлечения значения, а PutValue использует ту же reference. Checked TDZ slots и точная method commutation proof предотвращают раннее чтение captured/mapped/eval/derived-this операндов. Нового runtime carrier, opcode или generic-call обхода нет. [Подробный proof](compiler-tdz-proof.md).

## Baseline profile

Before binary SHA `9b9f54616e092214218a0005d84357f7393b58e497b116d7fc025bdb9298dc08`; fixture SHA `0ea9fcf227e65b5aaee406ecbba7f86d7775d8c25ce14221483361b0931b9627`. Три pure runtime запуска — 6 969 self samples. Отдельный artifact launch — 2 449 self, из них 1 642 generated; attribution полная, outside/missing отсутствуют. Только leaf/self, ARM64 PC `offset & ~3`, интервалы строго `[start,end)`; runtime и artifact samples не складываются.

Before OrderedCollection — function 505, c45. Artifact generated self: entry 11, body 25, ordinary return 2, constructor completion 7, cold construct 2, всего 47. Pure runtime native self под ближайшим физическим f505 — 132; 129 у Generic ArrayConstruct и 3 у constructor receiver paths. Artifact native self — 40, все у Generic. Это self attribution, а не dynamic instruction counts. [Baseline](before-analysis.md).

## Final emitted code: pinned CLI

Final artifact capture на SHA `f5efe012...` подтвердил ранний emitted proof build4 (`e3405ef4...`). Финальный producer — [after comparison](after-comparison.json); intermediate artifact-only observation сохраняется отдельно и не заменяет final capture.

| OrderedCollection | Before | Final |
|---|---:|---:|
| Encoded bytecode | 33 B | 46 B |
| Native code | 2 376 B | 2 584 B |
| Private entry, обе receiver-proof ветки | 63 instr | 53 instr |
| Frame span после начала saves | 43 instr | 33 instr |
| Window setup/fill | 12 instr | 2 instr |
| Generic / LoadWindow IR | 1 / 1 | 0 / 0 |
| Actual inlineLowered | 0 | 21 |

Before Generic `[824,920)` выполнял 24 emitted инструкции: публикацию root/source recipe, alloc context, обязательный BLR `array_construct_alloc` на +0x384, completion decode и STR результата в VM window на +0x394. Следующий LoadWindow делал LDR на +0x398. Final LAB fit `[980,1076)` также содержит 24 emitted инструкции, но ноль BLR: исчезли обязательная C helper activation, её interop/status path и store/load round trip. Collecting cold miss `[1076,1156)` сохраняет 20 emitted инструкций, `jit_new_array` на +0x45c и canonical source/root/completion records.

Entry сохраняет прежний callee-save set. Window materialization/fill заменены zero register span и actual-arguments pointer; net −10 emitted инструкций. Reserved extent вырос 0xd0→0xe0 из-за canonical homes. Добавленная обязательная работа видна отдельно: LoadGlobalBinding — 46 emitted инструкций, CheckCellValue — 4. Размер тела вырос; final fixed-work ниже измеряет весь запуск, без attribution retired counts конкретному IR узлу.

Inlining подтверждается 21 actual accepted splice на девяти уникальных source sites в 12 code objects: depth 1 — 12, depth 2 — 9. Каждый charged cost — ровно 46 encoded-bytecode B, thresholds прежние. Candidate count — 41→41; foreign bundles содержат source-owned NewArrayEmpty и не содержат его Generic/LoadWindow. [Final offsets/events](after-comparison.json), [ранний независимый proof](intrinsic-only-f505-evidence.md).

## Source, reconstruction и GC

- NewError использует semantic source function для kind constant pool и realm prototype. Оба producers входят в один rooted message kernel; disposed source realm не получает ambient fallback. Старый публичный Runtime probe доказал caller-prototype defect во всех 3 tiers × 2 directions. Runtime fixture покрывает foreign own entries и moving GC; отдельная VM publication fixture покрывает inline constant pool в двух same-realm chunks. Fresh VM13 и runtime-empty13 прошли без failures.
- Reconstructed parent не принимает placeholder Success(undefined) за результат queued child. Существующие prepared-child ownership, Call PC и resume flag сохраняются до actual child success/throw/fatal; нового frame, flag или source carrier нет. Exact before depth-2 artifacts и Node/interpreter/Template parent columns записаны в [restored child proof](restored-inline-source-fix.md).
- Bare Array prototype принадлежит retained default-realm roots, включая parked owner. Canonical Array.prototype classification принадлежит immutable brand существующей ExoticSlots metadata; moving GC/snapshot/realm disposal не меняют category. Shared exact-array metadata read не вызывает getter; clone/worker читают data descriptors без JS/VM allocation, а NativeScope indexed Get и Set/Define используют rooted canonical boundaries. One ArraySetLength owner сохраняет coercion order, rollback и readonly length. Fresh VM13, runtime-empty13, clone13 и worker13 прошли без failures.
- `builtin_to_string_tag_value` возвращает один `Result<&'static str, VmError>`; `Interpreter::is_array` одновременно распознаёт actual Array, branded canonical Array.prototype и Proxy target, передаёт revoked exception. Второй Proxy walker и отдельный Native tag reader удалены. Native Object toString, Array fallback и proven conversion используют один classifier; scoped driver определяет builtin tag до observable `@@toStringTag` Get. `array_to_string` перечитывает actual receiver из handle после collecting `join` Get. Getter получает actual receiver/Proxy, returned tag не остаётся raw через последующую allocation; revocation из tag getter не пересчитывает уже выбранный builtin tag.
- Primitive boxing выбирает `Intrinsic::{BooleanPrototype,NumberPrototype,StringPrototype,SymbolPrototype,BigIntPrototype}` непосредственно из traced realm roots, без mutable global constructor/prototype read. Object `new_target_is_self` сверяет NewTarget с canonical `IntrinsicCallKind::Object` callable, а wrapper allocator держит primitive/prototype/receiver в NativeScope. Замена либо null global binding не меняет ToObject или active constructor identity. Расширенный `array_brand_to_string.js` содержит 35 checks / 448 controlled allocations; strict collecting boxed-getter proof идёт через Array fallback, где дополнительный oracle совпадает. Native direct primitive getter следует primary ToObject specification; отличие внешнего oracle отмечено отдельно в [brand/order proof](array-brand-spec-review.md). Все восемь fresh corpus modes/strides прошли, raw outputs совпадают.
- `graph/frame.rs::allocation_exit_reason` выдаёт `ReceiverAllocationMiss` только для `Kind::NewReceiver`; другие allocation nodes получают `AllocationMiss`. ARM64/x86 передают тот же typed reason в существующем SideExit. `Interpreter::note_receiver_allocation_exit` сравнивает reason и не декодирует source bytecode; ordinary `New` для Array value больше не ошибочно считается receiver miss. Fresh VM/JIT host13, JIT x865 и runtime13 прошли без failures; diagnostics fixtures сохраняют exact actual receiver count.

Исторические failed JSON/logs остаются history. Worker12 имел exit 101 и 2 passed / 1 failed: test child ожидал marker из awaited script completion. Fixture теперь присваивает message payload realm-global marker и делает отдельный eval этого marker. Это проверяет actual worker payload, без предположения о универсальном `run_script` completion `undefined`; production API/behavior не менялись. Worker13 прошёл 3/0. Более ранние phase/receiver/realm fixture repairs не выдаются за прошедшие historical attempts.

## Финальный evidence

Текущий full difftest corpus total — **145**, включая пять новых файлов: `intrinsic_call_proofs.js`, `method_argument_order.js`, `restored_inline_completion.js`, `switch_tdz_method_order.js`, `array_brand_to_string.js`. Final host/x86 — **145/0 на каждом**, общий seed `340717225298`, timeout 120 000 ms. Все 145 distinct cases имеют interpreter oracle exit 0, кандидаты exit 0, без timeout/mismatch. `run_observations.py oracle` перечисляет ровно эти пять корпусов, каждый с Node и семью Otter режимами/strides: 40 наблюдений. `run_fixed_work.py` отдельно измеряет все 13 бенчей. Оба driver требуют `--expected-binary-sha`, отвергают чужой binary до запуска и сверяют terminal SHA с тем же expected.

`oracle-final.json` завершён **PASS 40**, CLI SHA и expected SHA равны pinned `f5efe012...`, `binary_unchanged: true`. Для каждого корпуса восемь exit 0 observations: Node, interpreter, normal native, Template, native/interpreter stress 256, native/Template stress 1. Raw stdout/stderr hashes и bytes совпадают; Node record's corpus SHA совпадает с current source. Raw stdout `array_brand_to_string` содержит 35 checks / 448 allocations и exact getter/order counters. Это correctness evidence, не instruction/performance measurement.

| Прочитанный final raw record | Exact result |
|---|---|
| `build-host-final-13`, `build-x86-final-5` | exit 0 / current freeze |
| `clippy-host-final-10`, `clippy-x86-final-5` | exit 0 / current freeze |
| `vm-lib-final-13` | 1236 passed / 0 failed, full unfiltered lib |
| `jit-lib-final-13`, `jit-lib-x86-final-5` | 260/0 host, 282/0 x86, full unfiltered lib |
| `runtime-array-construct-final-13`, `runtime-empty-allocations-final-13` | 9/0 и 10/0, два разрешённых test bins |
| `runtime-clone-final-13`, `runtime-worker-brand-final-13` | 9/0 (288 filtered) и 3/0 (294 filtered), exact named lib filters |
| `difftest-host-final-final-1`, `difftest-x86-final-5` | 145/0 host и 145/0 x86; seed 340717225298 |

Все перечисленные raw command records имеют exit 0 и exact freeze `6d01ab0d...`; final [collector](gate-summary.json) завершён PASS, 148 source paths, тот же digest. Full-e runner имеет отдельного Cargo producer: combined build13 SHA `661c6ffc...` pre-full сохранён в `combined-build-identities.json`, а `final-binary-identities.json` уже содержит actual post-full runner. CLI остаётся independently pinned.

Единственный `full-2026-10-11e.json` содержит **53 575 total / 52 463 passed / 12 failed / 1 100 skipped / 0 timed_out / 0 OOM / 0 crashed**. [Validator](test262-validation.json) имеет `status: passed`, canonical rows/rollups valid и identical failure set с b и d baseline MD; timeout paths и rechecks пусты. Full driver timeout — 7 200 000 ms, per-test timeout — 5 000 ms. Completed full report и actual `target/release/otter-test262` совпадают по SHA `60e9db994e23a99358ea688235da5bb00346bf0ce818452137beba02437d3be8`; validator before/after identities и raw full-report hash также unchanged. Это producer full-e, а не pre-full combined runner.

`gc-final.json` завершён **PASS 26** на pinned `f5efe012...`, `binary_unchanged: true`. Это normal + `OTTER_GC_STRESS=256` для всех 13 fixed-work fixtures, включая обязательные ts/earley-boyer/crypto/zlib и затронутые benchmarks. Каждый exit 0, raw hashes проверены, outputs каждой normal/stress пары совпадают. Это GC correctness observation; timing не используется для performance claim.

Reviewed compiler fixture fixes не ослабляют контракт: каждый compile проходит bytecode/context-chain verification; split method связывает CallWithThis с exact loaded-callee register, destructuring extraction — с source register, immutable rejection — с TypeError kind и точным message. Strict-global CFG fixture считает branch target от `pc + 1` и проверяет, что selected with/eval hit обходит global snapshot, а fallback store потребляет сохранённый proof. Selected `compiler-lib-final-2` содержит 129/0 и exit 0; `bytecode-compiler-lib-final-1` содержит completed bytecode section 99/0, но command exit 101 и compiler section 126/3 остаются failed history. Эти старые records имеют unchanged marker без final freeze digest: только явно reviewed scope принимается collector, identity current freeze им не приписывается.

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

## Итоговый complete fixed-work cohort

Повторный **полный cohort всех 13 бенчей**, завершённый в 13:22:27 UTC на unchanged pinned CLI, принят итоговым. Ratio Otter/Node означает retired instructions, меньше лучше. Геометрическое среднее **2.392569906614916x → 2.3841258185276977x**, delta **−0.3529296286755246%**. Deltablue −3.0282824147774168%, Mandreel −0.8181547160020064%; Box2D +0.9220713686664217%. Все шесть положительных delta показаны без округления до unchanged: richards, earley-boyer, regexp, splay, navier-stokes, box2d. Это complete cohort, без подстановки отдельных Mandreel повторов.

| Бенч | Otter retired | Node fixed retired | Δ Otter к parent |
|---|---:|---:|---:|
| richards | 28,516,738,340 | 11,559,753,905 | +0.070583% |
| deltablue | 38,693,536,617 | 5,590,907,366 | -3.028282% |
| crypto | 4,669,431,285 | 2,918,126,968 | -0.260524% |
| raytrace | 34,646,837,461 | 8,371,358,844 | -0.039252% |
| earley-boyer | 29,687,727,933 | 18,688,380,783 | +0.053765% |
| regexp | 51,086,763,242 | 7,302,024,402 | +0.009218% |
| splay | 12,215,981,301 | 4,396,614,120 | +0.042920% |
| navier-stokes | 8,889,045,597 | 8,095,275,899 | +0.005371% |
| box2d | 13,678,336,900 | 5,062,573,423 | +0.922071% |
| gbemu | 26,277,633,096 | 8,221,150,072 | -0.189386% |
| mandreel | 19,041,246,285 | 13,823,039,865 | -0.818155% |
| zlib | 24,357,335,472 | 24,749,906,421 | -0.612386% |
| ts | 37,357,651,462 | 20,854,641,072 | -0.692883% |

Producer `fixed-work-identity.json`: status passed, captured_at `2026-10-11T13:22:27.454458+00:00`, CLI SHA `f5efe012...`, binary_unchanged true, совпадающие fixture hashes. Все 13 raw counters положительны, Node denominator совпадает с exact parent. Эти counts включают обязательную работу полного CLI запуска; sample profile/статический размер не приписывает delta отдельному IR узлу.

## Исторический complete cohort и Mandreel повторы

Первоначальный полный cohort дал geo 2.4130132918495186x (+0.8544529954205959%) и Mandreel 21 576 131 904 (+12.385531088657986%). Его семь положительных delta сохраняются как фактическое initial observation. **Причина Mandreel выброса неизвестна**; тот же frozen binary без production edits дал три отдельных отрицательных delta и затем −0.8181547160020064% в повторном complete cohort. Стабильная +12.385531% регрессия не воспроизвелась; compiler/Graph гипотезам эта причина не приписывается.

| Бенч первоначального cohort | Otter retired | Node fixed retired | Δ Otter к parent |
|---|---:|---:|---:|
| richards | 28,489,080,369 | 11,559,753,905 | -0.026474% |
| deltablue | 38,700,393,781 | 5,590,907,366 | -3.011097% |
| crypto | 4,697,001,778 | 2,918,126,968 | +0.328384% |
| raytrace | 34,660,108,680 | 8,371,358,844 | -0.000963% |
| earley-boyer | 29,785,858,817 | 18,688,380,783 | +0.384486% |
| regexp | 50,938,416,646 | 7,302,024,402 | -0.281191% |
| splay | 12,209,913,778 | 4,396,614,120 | -0.006770% |
| navier-stokes | 8,900,852,704 | 8,095,275,899 | +0.138206% |
| box2d | 13,814,863,692 | 5,062,573,423 | +1.929399% |
| gbemu | 26,358,628,677 | 8,221,150,072 | +0.118260% |
| mandreel | 21,576,131,904 | 13,823,039,865 | +12.385531% |
| zlib | 24,378,567,708 | 24,749,906,421 | -0.525750% |
| ts | 37,771,183,640 | 20,854,641,072 | +0.406401% |

Initial producers: [first-performance.json](first-performance.json), [first-fixed-work-identity.json](first-fixed-work-identity.json), `first-fw-raw/`, `first-fixed-work.stdout` и `first-fixed-work.stderr`. Итоговые producers: [performance.json](performance.json), [fixed-work-identity.json](fixed-work-identity.json), `fw-raw/`. Exact comparison обоих cohorts использует `../object-protocol-operation/performance.json` и тот же fixed Node denominator. Каждый cohort самостоятельный, mixed-cohort геометрия не используется.

| Mandreel отдельный повтор | Otter retired | Δ к exact parent | Exit |
|---|---:|---:|---:|
| 1 | 19,122,174,530 | -0.396616516529% | 0 |
| 2 | 19,037,252,608 | -0.838956939155% | 0 |
| 3 | 19,032,125,174 | -0.865664663958% | 0 |

[Raw repeats](mandreel-fw-repeats.json). Complete final cohort измерен заново целиком после этих проверок; один Mandreel counter в первый cohort не подставлялся. Найденный redundant identifier `resolve_ref` — отдельное compiler улучшение следующего coherent slice; ни influence этого walk, ни кодовая причина первоначального выброса этими measurements не доказаны.

Collector `gate-summary.json` — **PASS**, source freeze148/`6d01ab0d...`, current command/binary/source records согласованы с явно reviewed historical compiler/bytecode section provenance. GPG-коммит выполняется отдельным вызовом после review гейта; затем `git log --show-signature -1` проверяет G.
