# Прямая операция object-protocol ABI

Parent: `742c603b0bf7e2ff573e6495fb8c595dc6634575`.

## Контракт

Один VM-owned `ObjectProtocolValueOp` задаёт checked operation word для двух boxed operands. ARM64/x86, Template/Graph передают операцию непосредственно в стаб. Word полностью проверяется до RuntimeCall binding, counters и семантических эффектов. Старый object opcode decoder удалён; source/root records сохраняют GC, source realm, committed completion и reconstruction.

Из каждого object transition удалён выбор операции через published opcode → semantic source → safepoint/return record → function/PC → bytecode decode. Scalar source decoder сохранён у своих потребителей. Ни нового carrier, внутренней версии, adapter, generic обхода, runtime probe, env flag, TLS или mutable global.

## Доказательства

ARM64 method body `object_protocol_values`: 444 bytes/111 static instructions → 88/22. Старый selector body содержал ещё 149 static instructions и транзитивные lookup callees. Новый producer добавляет один MOV; стаб проверяет word и исполняет прежний kernel. Статические счёты не выдаются за dynamic retired count; встроенное method body не суммируется с outer stub.

Box2d runtime profile, 3 последовательных запуска: total self 3018→2983, object boundary 205→109, operation selector 104→0. Используются exact physical extents и только self samples. Сохранённые root/source records совпадают во всех 475 generated bundles; у 446 assembly идентична, у 29 добавлена операция перед прежним committed call. IR/deopt идентичны у 224 optimizing bundles; return→safepoint associations сохранены у всех 475 bundles.

## Fixed work

Финальное измерение всех 13 бенчей проведено после завершения наших сборок/проверок. Точные integer counters и binary/fixture SHA сохранены; Node reference неизменна. Under-load наблюдение отдельно сохранено и не заменяет финальное.

| Бенч | Otter G | Otter/Node | Δ инструкций |
|---|---:|---:|---:|
| richards | 28.4966 | 2.4652× | -0.0321% |
| deltablue | 39.9019 | 7.1369× | -0.0115% |
| crypto | 4.6816 | 1.6043× | +0.1235% |
| raytrace | 34.6604 | 4.1404× | +0.0369% |
| earley-boyer | 29.6718 | 1.5877× | -0.0547% |
| regexp | 51.0821 | 6.9956× | +0.0759% |
| splay | 12.2107 | 2.7773× | +0.1802% |
| navier-stokes | 8.8886 | 1.0980× | +0.0375% |
| box2d | 13.5534 | 2.6772× | -4.4960% |
| gbemu | 26.3275 | 3.2024× | +0.0279% |
| mandreel | 19.1983 | 1.3889× | +0.0553% |
| zlib | 24.5074 | 0.9902× | +0.0241% |
| ts | 37.6183 | 1.8038× | -0.4951% |

Геометрическое среднее: 2.401113176×→2.392569907× (-0.355805%). Измерены положительные дельты у восьми бенчей; все явно показаны. Box2d −4.495985% согласуется с удалённым selector path; sampling не определяет точную долю retired effect.

Финальный замер всех 13 бенчей и два дополнительных idle повтора: regexp +0.0376…+0.2268%, Splay +0.1404…+0.1802% к базе. У двух отдельных stats-запусков совпали allocation/GC root/slot/card work, GC counts и JIT compile/deopt counts; изменилось время пауз. Это не устанавливает причину instruction-разброса и не позволяет объяснить under-load дельты задним числом. Сохраняется прежняя Splay field-observation регрессия.

## Гейт

Test262: полный runner с parent timeout 7 200 000 ms, прежние 12 fail, 1100 skip; четыре Date timeout отдельно прошли с лимитом 30 000 ms. Эффективный итог 52463/12/1100, без оставшихся timeouts/crashes/OOM. Host/x86 difftest 140/0, seed 340717225298. VM 1224/0; JIT 250/0, x86 JIT 272/0; runtime object-protocol 3/0. Host/x86 clippy -D warnings. GC stress 256, все 13 бенчей с совпадающим stdout. Новый корпус: 41 проверка; Node/interpreter/native/Template/stress outputs совпадают.

Emitted fixture пришлось исправить только в cfg(test): пустой feedback выбирал Int32Compare; ARITH_OTHER теперь проверяет требуемую object operation. Production binary SHA не изменился. Полные команды, результаты и fingerprints — `gate-summary.json`.
