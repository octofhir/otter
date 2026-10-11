# Store observation: контракты и доказательства

Parent `3e6f808ccc031bee2c3eb029f33527a577ff2246`. Результаты гейта и
retired counters: [gate-summary.json](gate-summary.json),
[performance.json](performance.json). Sampling, emitted counts и retired
counters имеют разные знаменатели и не складываются.

## Монотонное включение

Defining owner `o` имеет captured категории `C(o)` и live `L(o,t)`;
monotone merge даёт `C ⊆ L` на весь срок generation. Если `B'` ограничивает
значение после эффектов store, то при `B' ⊆ C` merge не меняет `L`:
observation удаляется без live equality, dependency или post-call
invalidation. Object membership сравнивает rooted `stored_shapes`, nullish —
биты captured word. Top absorbing. `FieldStoreOwner::select`
(`otter-jit/src/field_store.rs`) — единственная реализация правила.

## Где берётся `B'`

`Builder::store_value_bound` (`graph/builder_field_type.rs`) читает только
доказательства, которые путь уже имеет: current exact layout (≤2 shapes),
null/undefined constant, либо `field_values` bound, чьи owner guards уже
в `required_field_guards`, а targets — в текущем `stable_checked`. Он не
создаёт node и не demand guard. Own store сохраняет прежнюю demand policy,
но record формирует тот же `select`.

Append `S→T` (`Builder::named_store_owners`): authority по `T`; тот же SSA
value — `{T}`; bound c `S` — `R∪{T}` (шире двух shapes → observer); bound
без `S` доказывает non-alias. Overwrite сохраняет `R`. Literal candidate
рождается после inputs, fit не collects: `B' = R`, выбор до
`add(NewObjectLiteral)`, который clobbers layouts и `stable_checked`.

## Один carrier

`Kind::StoreOwnField { owners: (receiver_shape, record) }`,
`Kind::StoreNamedProperty { byte_pc, owners }` (по program),
`Kind::NewObjectLiteral { fields: Option<per input> }`. ARM64/x86 encoders
потребляют record; поиск `field_types`, destination shape и fallback пустой
membership из emission удалены. VM literal geometry не несёт owners; bank
`bake_field_types` — единственный authority, отсутствие record → committed
birth. `emit_observe_known` принимает только наблюдающий record.

## Generic cached store

`StorePropertyCached.barrier` = `Builder::owes_barrier`: fresh receiver —
только `NewReceiver` ordinary cell, т.е. его storage = он сам; never-cell
значение не требует barrier ни для какого owner. Hit без barrier идёт прямо
в `done`; collecting miss по-прежнему выполняет canonical store со своими
barriers. `emit_select_entry` оставляет ordinary receiver на fall-through,
Array/closure/native projections — out of line, каждый возвращается в scan.

## Executable/structural proofs

- Builder: append alias matrix (exact alias, disjoint, possible alias
  captured `S` без `T` / с `T`, unknown, Top), overwrite/nullish matrix,
  literal selection до birth и без authority — все с 0 retained
  `CheckFieldTypes`/`CheckStableShapes`.
- ARM/x86 emitted: all-zero literal без `field_type_observe` и без второго
  LAB buffer reload; observing record — оба присутствуют.
- VM: literal bank owners равны `field_type::owner` для каждого поля.
- Runtime `jit_empty_allocations`: typed literal plan присутствует в
  реальных OSR/moving-GC captures (`NewObjectLiteral { fields: Some([`).
- Difftest `field_store_owner.js`: self-alias append, possible alias,
  rotate из loaded fields, literal из known value, null→undefined widening,
  Array bag overwrite, generic stores чисел/fresh receivers, effects once;
  Node = interpreter = native = Template, stress1/256.
