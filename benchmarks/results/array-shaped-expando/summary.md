# Array named properties: shaped expando

Неиндексные строки и Symbols Array хранятся в обычном shaped expando-object.
У RegExp match metadata больше нет отдельного named-map и обязательного exotic
sidecar. Final-shape bag рождается с готовыми index/input/groups, а /d сохраняет
identity capture pairs. Три typed slots существующего VM layout cache удаляют
повторные intern/lock/hash/Arc ladders и schema discovery из warm match.

Array→bag edge, bag→field barriers, descriptors, enumeration/deletion, prototypes,
GC и snapshot используют существующих обычных владельцев. Bag — storage;
receiver/source/deopt completion остаются исходным Array. Один VM-owned projection
codec и одна geometry обслуживают ARM64/x86. Общий wide-null guard adjacent
rare/expando handles заменил две независимые проверки.

## Измеренная удалённая работа

Actual ARM64 paired source показывает GetIterator guard 4→2 инструкции:
один load и один branch исчезли. Template hit 30→28; Graph Generic hit 33→31.
Source stamps, cold helper/status и root/deopt recipes совпали; Generic и
LoadWindow сохранены. Три current native named Array sites имеют по 24 инструкции
projection/shape/bank без property helper. Это capability proof: original
map→expando before warmed named pair отсутствует, поэтому точный выигрыш старого
named hit здесь не заявляется. Cell48 и birth zero stores не уменьшились.

Подробности контрактов и реального moving-GC/OOM proof:
[proof-report.md](proof-report.md). Raw source/body/tier/PC пары:
[packing-comparison.json](packing-comparison.json).

## Fixed-work: весь завершённый cohort

Один последовательный запуск `bash scripts/dev/fw_vs_node.sh target/release/otter`
на idle host, все 13 fixtures, unchanged CLI SHA
`aaf94e6c7f4bc4fa1479396d578d110f7fdb646e555d396a4aaaba3763fbe9af`.
Reference counts и fixtures не менялись. Δ — retired instructions относительно
подписанного parent `f83b59af82619f48f4f8b5900ad2e4d3038690ee`.

| Бенч | Otter/Node | Δ инструкций к parent |
|---|---:|---:|
| richards | 2.463946x | -0.119700% |
| deltablue | 6.820960x | -1.442567% |
| crypto | 1.594956x | -0.324377% |
| raytrace | 4.138565x | -0.004105% |
| earley-boyer | 1.583971x | -0.289241% |
| regexp | 6.615723x | -5.438969% |
| splay | 2.785967x | +0.268840% |
| navier-stokes | 1.097375x | -0.061818% |
| box2d | 2.711970x | +0.374394% |
| gbemu | 3.168576x | -0.868772% |
| mandreel | 1.374637x | -0.207868% |
| zlib | 0.983807x | -0.033657% |
| ts | 1.803771x | +0.694213% |

Геометрическое среднее **2.384125818528→2.370180835017**, **−0.584910%**.
Все положительные дельты: **Splay +0.268840%, Box2D +0.374394%, TS +0.694213%**.
RegExp **−5.438969%** к parent; **−6.425154%** к сохранённому source6.
Splay налог требует следующего field-observation среза и здесь не объявляется
устранённым. Это instruction ratios, не wall-clock ratios.

Предыдущий source6 прошёл полный correctness gate, но all13 имел geo +0.078602%
и RegExp +1.053899%; coherent cache/guard followup выполнен до коммита.
Оба полных source6 cohorts и isolated repeat сохранены. Первый Mandreel +12.34%
не воспроизвёлся; его причина неизвестна. Итоговый cohort не смешан с repeats.
Exact Mandreel bytecode equality parent/source6 проверена отдельно;
[performance.json](performance.json) содержит текущие integer counters.

## Профиль RegExp: sampled time, не retired

Свежие captures текущего CLI: pure runtime — 3 последовательных запуска,
artifact — 1 запуск. Учитывается только exclusive self; пересекающиеся extents
не добавляются. Parent→final pure self **11703→10512**, artifact **4499→4070**.
Старые Array named/rare категории ушли в 0: named-map hash/insert/other
253/80/13, old match named birth 40, unresolved string-map 36,
indexed-or-old-rare allocation/trace/drop 265/49/5 (включает индексный sidecar,
поэтому не только named расход). Новые владельцы: shaped match birth 300,
projected property runtime 78, managed bag birth 38. Таблица:
[after-comparison.md](after-comparison.md).

Source6→final выделен по точным leaf names
(`key_tax_leaves.py` → `key-tax-leaves.json`): HostAtom intern, SipHash,
Arc clone/drop, atomic CAS/load, `record_layout_for_atoms` и
`null_object_layout_for_atoms` — **386→4** self samples; весь pure total
10941→10512. Остаток 4 — один SipHash write и 3 samples
`Result<ObjectLayout>::branch` warm slot path. Generic property samples не
приписываются Array: в 32 однозначных RegExp generation pairs нет ни одного
emitted `LoadArrayExpando` site ([after-source-comparison.md](after-source-comparison.md),
integrity errors 0/0). Выигрыш RegExp здесь — VM match birth/layout, не JIT named hit.

## Найденный emitted налог

Graph generic `StorePropertyCached` теперь сам выполняет value barrier с
фактическим storage owner; builder больше не добавляет отдельный `WriteBarrier`
node и поэтому теряет его never-cell/fresh elision. ARM64 `Exec` byte30
(тот же source/body SHA): ordinary own overwrite hit константы получил **+5**
emitted instructions на пути: `mov/b` storage selection, `mov` actual owner и
`tst/b.ne` никогда не нужного barrier filter. Region 1600→1828 bytes; append
path ещё проверяет projection bits entry. Retired доля этого пути не измерена;
он не объявляется причиной Splay/Box2D/TS дельт.

## Correctness

Один полный background safe test262 runner с timeout7200000 завершился:
53 575 total, 52 463 pass, **12 fail**, 1100 skip, 0 timeout/OOM/crash.
Failure set точь-в-точь совпал с full-2026-10-11b/e.md.
Host и x86 difftest **148/0**, timeout-ms120000, без таймаутов;
VM lib **1244/0**, JIT host **265/0**, x86 **288/0**.
Clippy host/x86: `-D warnings`, exit0. GC stress256: обычный и stress запуск
всех13 завершились успешно с идентичным выводом. Runtime Array proofs9/0 и
LAB/OSR/root/source proofs12/0; layout13/0, compiler137/0, bytecode99/0,
regex23/0, clone9/0, worker-brand3/0. Три новых корпуса имеют 87/22/16 checks;
Node, interpreter, native, Template и moving-GC modes совпали.

Исправлены обнаруженные existing defects: /d pair identity, двухфазный
DefineProperties с Symbols/holes, receiver при Array inheritance, explicit-null
prototype/cycle rejection и return из collecting SetPrototypeOf, Proxy captured
targets при self-revocation, rooted template arrays и fallible indexed state.

Финальная freeze —87 путей,
`5a8419e47f2c649478c1fbaab11b57bf2d42992bd080065c4769168822063007`.
Safe runner SHA `e7029ec5737bd6842eb5d0085172e547ed351cb005b5b08b7d2f2b296ec010a8`.
Exact producer SHA проверяется в [gate-summary.json](gate-summary.json);
эта строка human-readable не заменяет machine provenance.

