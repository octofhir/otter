# Array push/pop as Graph nodes

Parent `e1853b51c2129f7deca24db5e81873507881c1e3`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

Deltablue до среза: каждый `push`/`pop` guarded-сайта в Graph шёл через
`Generic` (Template-операция в окне регистров): guard'ы, сохранение живых
регистров, C-вызов `array_push_leaf`/`array_pop_leaf`, внутри — `len`,
`dense_capacity`, `dense_range_is_fast`, `reserve`, `refresh_element_cache`,
`mark_dirty`, `record_array_write`; при полном буфере ещё один C-вызов
`array_push_alloc`. Профиль: push/pop runtime ≈ 18–19% всех сэмплов.
`Generic` к тому же запрещён в inline-теле, поэтому `OrderedCollection.add`
и `removeFirst` не встраивались.

## Архитектура

- IR: `ArrayPush(byte_pc)` / `ArrayPop(byte_pc)`. Eager deopt, если receiver
  не Array с pinned `%Array.prototype%` нужного hidden class и builtin в слоте
  (общий `emit_intrinsic_method_identity`, им же теперь пользуется `f.call`);
  для push — ещё array-index protector.
- Inline: tagged storage, dense prefix = весь `length`; push в свободную
  ёмкость, pop присутствующего последнего элемента (слот ← `undefined`).
  Shell dense len, slab len и `length` двигаются вместе.
- Всё прочее (рост, пустой массив без slab, numeric, дырки, хвост длиннее
  dense prefix, sealed/frozen length) — builtin через committed
  `STUB_JIT_OBJECT_PROTOCOL_VALUE` (`ObjectProtocolValueOp::ArrayPush/ArrayPop`).
- Узлу не нужен кадр: inline-тела с push/pop остаются inline.
- Element write barrier (обе архитектуры): молодое значение расширяет dirty
  interval slab inline; stub вызывается только при marking или для старого
  незапомненного slab. Раньше stub звался на каждое молодое значение.

## Fixed-work (весь cohort)

CLI `6ed3a684aca6f370c59cc942a8d6fcd84ac7a18b148d0e60d948943ef455b4f8`.

| Бенч | Otter/Node | Δ к e1853b51c |
|---|---:|---:|
| richards | 2.460366x | -0.018190% |
| deltablue | 6.115264x | **-10.339773%** |
| crypto | 1.595009x | -0.075720% |
| raytrace | 4.138408x | +0.000697% |
| earley-boyer | 1.584485x | -0.060855% |
| regexp | 6.602520x | -0.001373% |
| splay | 2.100512x | -0.043716% |
| navier-stokes | 1.097945x | -0.036366% |
| box2d | 2.705579x | -0.176434% |
| gbemu | 3.174908x | +0.007071% |
| mandreel | 1.375540x | -0.010641% |
| zlib | 0.983548x | -0.005077% |
| ts | 1.793621x | -0.226038% |

Геометрическое среднее **2.318920696598→2.298389057858, −0.885396%**.
Регрессии: gbemu +0.007%, raytrace +0.001% (шум одиночного прогона).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-11i`. Difftest host/x86 151/0 (новый
`array_push_pop_inline.js`). VM 1245/0, JIT 270/0, x86 293/0, clippy host/x86.
GC stress 256 всех 13: вывод совпал. Корпуса совпали с Node во всех режимах.

