# Store observation: один выбранный record на native store

Parent: подписанный `3e6f808ccc031bee2c3eb029f33527a577ff2246`.
Source freeze: 27 путей; machine provenance — [gate-summary.json](gate-summary.json).

## Что изменилось

- Один JIT leaf `FieldStoreOwner { owner_shape, stored_shapes, nullish_mask }`
  (`crates/otter-jit/src/field_store.rs`) для own, named и literal stores.
  `owner_shape == 0` — observation отсутствует целиком: captured Top либо
  post-effect bound `B ⊆ C ⊆ L`.
- Builder выбирает record до эффекта. Named store: по одному record на
  program, authority destination-shape `T`; append `S→T` — тот же SSA value
  = `{T}`, bound с `S` → `R∪{T}`, bound без `S` → `R`. Literal: per-input
  record до collecting `NewObjectLiteral`. Pure selection: current exact
  layout, nullish constant, либо field bound с уже demanded guards и
  `stable_checked` targets. Новых `CheckFieldTypes`/`CheckStableShapes` нет
  (builder tests проверяют 0 retained checks).
- ARM64/x86 encoders больше не ищут destination shape/field/`field_types`;
  VM geometry `JitObjectLiteralAllocationPlan::field_owners` удалён, bank —
  единственная authority. Нет record → только committed birth.
- Graph generic `StorePropertyCached` получил builder-proved `barrier`:
  never-cell значение или fresh ordinary receiver не платят value barrier.
  Projected receivers (Array bag, function bags) выбираются out of line:
  ordinary hit больше не делает taken `b keyed`.

## Emitted

- RegExp `Exec` byte30 ordinary overwrite hit константы: налог
  `3e6f808cc` к `f83b59af8` был +5 instructions, теперь +1 (`mov` storage).
  Region 1828→1776 bytes; `barrier: false`.
- Splay hot `<anonymous>` bodies: `field_type_observe` relocations 30→26 и
  10→4, code 31456→28816 и 14312→10352 bytes. `GeneratePayloadTree` без
  изменений: `{left,right}` берут call results без bound.
- All-zero literal: ни observer leaf, ни второй LAB buffer reload
  (ARM/x86 emitted fixtures).

## Fixed-work: весь cohort

Один запуск `bash scripts/dev/fw_vs_node.sh target/release/otter`, idle host,
CLI `5fdcf8de15105c92283bcfe72c8d67c8a791b65bd365e092c831d93764fc0a43`.
Δ — retired instructions к `3e6f808cc`
([performance.json](performance.json)).

| Бенч | Otter/Node | Δ |
|---|---:|---:|
| richards | 2.460734x | -0.130344% |
| deltablue | 6.818304x | -0.038944% |
| crypto | 1.594783x | -0.010843% |
| raytrace | 4.139899x | +0.032216% |
| earley-boyer | 1.584006x | +0.002185% |
| regexp | 6.613382x | -0.035378% |
| splay | 2.779305x | -0.239130% |
| navier-stokes | 1.097890x | +0.046969% |
| box2d | 2.708493x | -0.128222% |
| gbemu | 3.168170x | -0.012806% |
| mandreel | 1.374693x | +0.004048% |
| zlib | 0.983401x | -0.041326% |
| ts | 1.800132x | -0.201738% |

Геометрическое среднее **2.370180835017→2.368806510821, −0.057984%**.
Положительные дельты: raytrace +0.032216%, earley-boyer +0.002185%,
navier-stokes +0.046969%, mandreel +0.004048%. Splay в одиночных повторах
того же CLI колебался 12.184–12.284G retired: ±0.5% — шум GC/JIT timing;
цифра таблицы — только из полного cohort.

## Splay профиль до среза

Свежий capture `3e6f808cc` (10 pure runs): GC 60.1% self samples, optimizing
JIT 28.4%. Inclusive minor GC 55%, вызывается из committed literal stubs при
LAB miss. Этот срез убирает observation work, но главный обязательный
расход Splay — копирование и трассировка выживших в scavenger.

## Correctness

test262 full safe runner: 53 575 / 52 463 pass / **12 fail** / 1100 skip,
0 timeout/OOM/crash; failure set идентичен `full-2026-10-11g`.
Difftest host/x86 **149/0** (новый `field_store_owner.js`), timeout-ms 120000.
VM 1244/0, JIT host 269/0, x86 292/0, runtime `jit_empty_allocations` 12/0.
Clippy host/x86 `-D warnings`. GC stress 256 всех 13 fixtures: вывод совпал.
Три корпуса (`field_store_owner`, `field_shape_tracking`,
`array_named_expando`) совпали с Node в interpreter/native/Template и
stress1/256 режимах: 21/21.
