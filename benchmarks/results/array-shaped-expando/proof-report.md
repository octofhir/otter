# Array shaped expando: контракты и доказательства

Parent: подписанный `f83b59af82619f48f4f8b5900ad2e4d3038690ee`.
Source freeze: 87 путей,
`5a8419e47f2c649478c1fbaab11b57bf2d42992bd080065c4769168822063007`.
CLI SHA256: `aaf94e6c7f4bc4fa1479396d578d110f7fdb646e555d396a4aaaba3763fbe9af`.
Финальные correctness и retired результаты: [gate-summary.json](gate-summary.json)
и [performance.json](performance.json). Sampling, static emitted counts и
retired counters имеют разные знаменатели и не складываются.

## Один storage и semantic owner

Actual Array хранит неиндексные строки и Symbols в одном ordinary Object с null
prototype. Nullable compressed `ArrayBody.expando` — единственная Array→bag связь;
у массива без этих свойств bag=null и дополнительный Object не рождается.
`ArrayPropertyKey::{Length,Index(u32),Expando}` — один классификатор ключей.
Bag владеет slots/descriptors/symbol table и физическими write barriers;
исходный Array остаётся receiver для accessors, inherited Set, extensibility и
source completion. Его prototype не заменяется null prototype storage bag.

Named Define использует существующий ordinary partial-descriptor алгоритм
(`object_internal_ops/array_define.rs:80`), а Array-only named map/descriptor
алгоритмы удалены. Rare indexed sidecar остаётся для sparse elements, indexed
accessors/flags, length writability, extensibility, captured JSON bytes и
prototype override. Четыре allocating indexed setters и integrity preparation
возвращают существующий OOM carrier; cap refusal не превращается в panic.

Lifetime brand настоящего Array.prototype остаётся в существующем Object owner.
Shared ArraySetLength сохраняет два отдельных coercion steps, numeric length,
truncation/nonconfigurable rollback и окончательное writable=false. Новый bag
не заменяет Array exotic length/index semantics брендированного прототипа.
Ordinary lookalikes не получают brand через assign/prototype change.

Body теперь `repr(C)`, 40 bytes / alignment8; header-inclusive shell48. Rare24 и
bag28 образуют один выровненный 8-byte word без padding/slab. Whole inherited
lookup проверяет оба null одним Word64 guard. Dense present-index proof по-прежнему
не проверяет bag=null: indexed semantics и named shadow — разные обязанности.
GC посещает slab/rare/bag отдельными 4-byte slots и только затем обновляет cached
element base (`array.rs:130`). Packed word не является новым GC carrier.

## Rooted birth, сильный layout cache и snapshot

`handles.rs:539,561` ensure/attach перечитывают Array и bag из существующей handle
arena. Attachment публикует одну Array→bag связь и её generational barrier.
Ordinary Object field/slab/symbol owners отвечают за дальнейшие child barriers.
Named writes не загрязняют indexed JSON bytes; semantic JSON owner выполняет
observable Get(toJSON) до использования indexed cache.

`ObjectLayoutCache` остаётся одним сильным owner. Dynamic plain/atomized layouts
partitioned по prototype root и ordered keys. Три fixed null-prototype RegExp
slots (`Match`, `MatchWithIndices`, `Indices`) содержат тот же ObjectLayout DTO.
`shape_ids()` включает оба вида entries в один root walk; fixed slot не создаёт
дубликат в dynamic map. `handles.rs:485` возвращает warm token до runtime-root
guard, HostAtom, root preparation и signature lookup. Cold miss строит complete
null-root lineage и только затем публикует единственный slot.

Null-root принадлежит isolate heap, поэтому fixed slots безопасно общие для
default/additional realms и не удерживают их Object.prototype. Actual Array
prototype birth остаётся существующим source-realm allocator owner. Completed
runtime turns снимают temporary shape pins: lifetime final shape действительно
обеспечивает layout cache, в том числе без экземпляров и после full GC.

Snapshot переносит bag/shape/fields/symbols в существующем page image с обычной
relocation. Единственный foreign Array DTO — sorted numeric flags и scalar length
writability (`array/indexed_snapshot.rs:20`). Named host clone/rebuild, прежние
DTO/readers и версии не остаются. Unsupported sparse/accessor/JSON foreign payload
по-прежнему отказывает. Restore сбрасывает весь derived ObjectLayoutCache через
Default, включая fixed slots, а не вводит второй snapshot формат.

## Исчезнувшая обязательная работа

| Путь | Удалено | Остаётся |
|---|---|---|
| Default-realm match без /d | Metadata ArrayExotic cell, IndexMap/reserve, три owned String key и map inserts, отдельные metadata stores | Один final-shape bag с 3 initialized fields и attachment |
| Match с /d | Два metadata sidecar/map, пять owned String key и map inserts | Result bag4 + indices bag1 |
| Повторный match без /d | Пять HostAtom intern/lock/hash/Arc ladders, root preparation/id lookup, generic cache hash и ordered-key comparison | Прямое чтение Match slot |
| Повторный match с /d | Семь atom ladders и два layout discovery | Прямое чтение MatchWithIndices и Indices slots |
| Capture pairs / indices | Post-birth indexed stores | Final initialized array_from birth |
| Named /d capture | Дополнительный pair-array birth | Уже rooted numbered capture pair с точной identity |
| Доказанный named Array hit | Array-only map hash/lookup/runtime property boundary | Bag projection + ordinary shape/field hit |
| Numeric descriptor metadata | Conversion в строковый ключ и string-map lookup | Numeric u32 authority |
| Whole inherited Array lookup | Один дополнительный 32-bit memory read и conditional branch | Один aligned 64-bit zero guard |

Extra-realm/subclass prototype overrides остаются законной rare-state работой;
отсутствие metadata sidecar не означает отсутствие любого Array sidecar.
Matching, capture strings, Arrays/slabs, dynamic named groups, actual bag birth,
field seeding, attachment/field barriers и cold committed completion остаются.
`field_type::observe_owner` уже отсекает absorbing TOP, однако generic seed
lineage walk и stable groups merge пока сохраняются. Этот налог требует shared
monotonic field-observation proof, а не отдельного RegExp observer обхода.

## Исправленные существовавшие дефекты

`Match::named_group_index` выбирает фактический matched duplicate-name alternative
либо первый declared index, когда все unmatched (`otter-regex/src/api.rs:431`).
`indices.groups.name` ссылается на тот же pair, что `indices[index]`; одинаковые
ranges разных captures не смешивают identity.

Object.defineProperties собирает только own enumerable source keys, включая
Symbols и исключая Array holes. Все descriptor conversions заканчиваются до
target definitions; key/value/getter/setter остаются rooted через collecting
getter/trap. Property-key conversion сохраняет WTF-16, а named inherited setters
получают реальный Array receiver. Ordinary/private Symbol storage сохраняет
descriptor attributes, chronology и Private Name exclusion из reflection/freeze.

Proxy Get/GetOwnProperty/Has/IsExtensible/PreventExtensions сохраняют captured
target через reentry и self-revocation. Descriptor payload и pending key/value
rooted; nested descriptor/extensibility traps исполняются в semantic order без
replay. Это устраняет stale-target/root gaps в уже существовавших owners.

Prototype storage отличает explicit null от absent override. Canonical scoped
SetPrototypeOf (`object_internal_ops/prototype_set.rs:72`) выполняет SameValue,
extensibility и ordinary cycle walk до publication. Proxy/namespace останавливают
этот walk без пользовательского GetPrototypeOf trap. Class constructor/static
links публикуются после успешной проверки. Native Object.setPrototypeOf
(`object_statics.rs:512`) возвращает receiver из текущего Local после collecting
Array sidecar allocation, сохраняя identity и движущийся prototype.

## Один владелец carrier и geometry

`PropertyStorageProjection` принадлежит VM: `Receiver=0`, `LookupStart=1`,
`ArrayExpando=2`. Один checked codec занимает low2 bits shape key; reserved3,
null и недопустимая alignment отвергаются. IC, CacheIR, snapshot и оба native
backends используют этот enum, а не отдельные lookup flags.

VM-owned `JitArrayLayout` передаёт tag, length byte и expando byte, производимые
из живой geometry. `LoadArrayExpando` перечитывает nullable compressed edge и
создаёт tagged bag SSA. Shape и banks принадлежат bag; source Array остаётся
receiver. Shape-feedback обычных объектов не отсекает эту projection по типу
исходного Array.

Один `JitIntrinsicPrototype.guard: Option<JitBodyGuard>` описывает inherited
receiver proof; отдельный Array tuple guards удалён. Whole Array proof использует
Word64 zero; другие receiver families сохраняют свои predicates. GC трассирует
оба 32-bit edges независимо: wide null predicate не является pointer carrier.

## Обязательные инварианты ARM64 и x86

- Array projection допускает только existing own data load/overwrite.
  Null prototype bag не доказывает Array absence, inheritance или append.
  Megamorphic/shared-table fallback не принимает Array за ordinary bag receiver.
- Type/null/projection/live shape/writability guards предшествуют эффекту.
  Miss сохраняет original Array и уже вычисленный RHS. Descriptor/accessor
  transition возобновляет исходный source PC; evaluation не повторяется.
- Bag SSA владеет field store, observation и write barrier. Barrier parent —
  actual bag; Array не подменяет этот owner. Moving collector видит Array→bag
  edge и bag→field edge через обычные trace/barrier owners.
- Tagged bag не становится JS receiver или source frame. Canonical homes,
  source records и eager deopt DTO восстанавливают исходный Array и RHS.
  Collecting cold boundary сохраняет существующие roots и committed completion.
- Wide inherited proof предшествует выбору retained prototype; последующие
  holder shape/descriptor/slot и validity proofs остаются обязательными.
  Dense own path не требует expando-null.
- Private JS entry и общий Frame contract не меняются. Noncollecting barrier
  использует существующий C leaf boundary; x86 C calls соблюдают platform ABI,
  private actual-only JS transfer остаётся отдельной границей.

Focused fixtures обоих backends проверяют live edge reload, source preservation,
returned bag barrier owner, null/type misses и отказ от projected append/read
как store. Wide-word fixtures отвергают отдельно low и high половины пары и
сохраняют receiver. x86 fixtures выполнены: JIT lib 288/0; полный x86 difftest 148/0 без
таймаутов. x86 emitted instruction counts здесь не измерялись.

## Actual ARM64 packing pair

[packing-comparison.json](packing-comparison.json) соединяет source/tier/entry/body/PC
без использования runtime-local IDs как cross-capture identity. Exact `-e`
source SHA `3ef601a70263d64f07d4482fb148f88856b17116a466ffa9e4468f133078b050`;
до/после exit0 и одинаковый Node output `64000 17 17 3 7`.

| `sumArray`, GetIterator PC2/byte20 | Before | Source7 |
|---|---:|---:|
| Template whole source instructions | 44 | 42 |
| Template ordinary Array hit | 30 | 28 |
| Whole pair guard | 4 | 2 |
| Graph Generic whole source instructions | 47 | 45 |
| Graph Generic ordinary Array hit | 33 | 31 |

Template PC stamp, realm proof, validity targets и один committed
`jit_iterator_op` сохраняются; cold call переместился на −8 bytes. Root record
и return-site array побайтово совпадают. Graph использует shared Template
emitter внутри Generic и получает тот же guard cut. Его IR/deopt/11 root
records совпадают, **Generic и оба LoadWindow остаются**.

## Текущая named capability и предел результата

В текущем capture три actual Array named chains — `readNamed` byte10,
`writeNamed` byte20 и его read byte46 — имеют по **24 ARM64 инструкции**:
10 projection + 10 shape/type proof + 4 bank operations. Packing меняет
bag byte32→28, а их counts остаются прежними. Это current native capability,
не измеренный выигрыш исходного map→expando перехода: before warmed named
Array pair для него отсутствует.

Read/write deopt и root JSON совпадают до/после packing. Store resume byte20
содержит original Array в register0 и evaluated RHS в register1. Actual
`WriteBarrier [bag,RHS]` остаётся noncollecting; separate cold branch передаёт
bag как C owner. Whole functions сохраняют 2/3 root records и zero JS return
associations. Детали: [emitted-findings.md](emitted-findings.md).

`loadJoin` остаётся `LoadPropertyCached(atom=None)` без доказанного pair guard;
этому site packing gain не приписан. Прежний RegExp capture не содержит warmed
`LoadArrayExpando` sites. Rust match birth не доказывает исчезновение горячего
Graph named helper или Generic. Source6 evidence сохранена в
[архивном отчёте](validated-source-6/emitted-findings.md).


## RegExp профиль и emitted pairs текущего CLI

Pure runtime 3 runs / artifact 1 run, exclusive self, extents не добавляются.
Parent→final pure **11703→10512**, artifact **4499→4070**; generated self
532→577 и 181→204 соответственно. Old named-map/rare/unresolved категории →0,
новые shaped match birth/projected runtime/bag birth = 300/78/38 pure samples.
Source6→final key/layout leaves (HostAtom intern, SipHash, Arc, atomic,
`record_layout_for_atoms`, `null_object_layout_for_atoms`) **386→4**: fixed slots
действительно убрали warm discovery. Gate collector пересчитал XML self tables,
frame partition и source comparison из сохранённых inputs без запуска producers.

Source comparison: 38/38 bundles, integrity errors 0/0, 32 one-to-one pairs,
3 ambiguous trigger groups. Ни в одной pair нет emitted `LoadArrayExpando`; Array
named JIT hit RegExp не затрагивает. Matched optimizing bodies выросли
(`Exec` 7096→7488, `runBlock9` 114056→117024 bytes), Template — на 32–72 bytes на
function. Emitted bytes включают cold branches и не являются retired work.

`Exec` byte30 `StorePropertyCached` показывает конкретный hot-path налог. Builder
перестал добавлять отдельный `WriteBarrier` после cached store
(`graph/builder.rs:3009`), а `emit_store_property_cached`
(`graph/arm64.rs:2929`) всегда выполняет value barrier с actual storage owner.
Never-cell и fresh-receiver elision тем самым потеряны. Ordinary own overwrite
константы: storage selection `mov/b`, owner `mov` (`arm64/property_ic.rs:383`),
barrier filter `tst/b.ne` — **+5** emitted instructions на пути; append ещё
проверяет projection bits entry (`arm64/property_ic.rs:335`). Region 1600→1828
bytes. Retired доля не измерена и не связывается с Splay/Box2D/TS дельтами.

## Исполняемые GC и birth доказательства

VM 1244/0 включает следующие точные GC/OOM/cache доказательства:

- `only_array_root_retains_young_bag_then_young_named_and_symbol_values`: inner
  handles отпущены; old Array→young bag и old bag→young field/Symbol остаются
  достижимыми только через Array. Actual minors/slot updates/offset motion.
- `full_bag_symbol_growth_moves_exact_edges_and_rejects_or_commits_atomically`:
  0/4/64×reclaimed false/true, cap−1, motion description+трёх children; OOM
  сохраняет source table/order/attrs либо reclaimed commit увеличивает table.
  Overwrite полного table не выполняет ненужную allocation/GC.
- `snapshot_relocates_managed_bag_and_rebuilds_only_numeric_indexed_records`:
  две реальные rooted minors подготавливают empty nursery; capture/restore/full
  GC сохраняют один alias в index/named/Symbol и numeric flags/length.
- `indexed_state_allocation_refusal_is_fallible_and_publishes_no_sidecar`:
  четыре setters проходят actual cap collection и сохраняют Array identity без
  частичной rare/bag/extensibility publication.
- `set_prototype_returns_current_array_after_collecting_sidecar_birth`:
  Object/null matrix; именно sidecar allocation вызывает full+minor GC, +1
  sidecar и slot updates; Array/prototype реально перемещаются, return/alias
  сравниваются с текущими handles до payload inspection.
- `fixed_layouts_survive_completed_scopes_and_full_gc_without_instances`:
  все 3 slots, cold default/extra-first, shared tokens, disposed extra realm,
  actual full GC без record instances; затем точные null-prototype/order/WEC/
  initialized values.
- `fixed_layout_hits_do_not_prepare_roots_or_dynamic_signatures`: 128×3 hits не
  меняют allocation/reservation/cycle counters или dynamic map; Default cache
  не содержит fixed tokens/root IDs.

Удалённые Array symbol-sidecar tests не означают потерю physical growth/OOM
coverage: общий ordinary symbol table — единственный storage owner; новая full
matrix доказывает его через Array edge, сохраняя cap/motion/atomicity assertions.

