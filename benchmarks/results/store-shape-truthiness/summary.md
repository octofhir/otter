# Named stores on proved receivers; truthiness of known values

Parent `c421afbcb`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

После встраивания конструкторов raytrace строит каждый объект цепочкой
`this.x = …` в свежий receiver, форма которого известна builder'у. Каждый
`StoreNamedProperty` всё равно проверял cell и тег объекта и сравнивал форму
receiver во всех программах сайта; каждый transition стирал все факты о
формах в функции. `x ? x : 0` над только что вычисленным double боксировал
значение и проходил полный `ToBoolean` по тегам.

## Архитектура

- `StoreNamedProperty { receiver_shape }`: при доказанной единственной форме
  запускаются только программы, которые её охраняют, без cell/object-проверки
  и без guard этой формы; результирующая форма — ровно её потомок, следующий
  store тоже доказан.
- Transition забывает только факты, называющие исходную форму
  (`forget_shapes_from`): узел с другой формой — другой объект.
- `JumpIfTrue/False`: доказанный обычный объект — ветка выбрана при сборке;
  значение с представлением Int32 — `Int32 != 0`; Float64 — новый
  `BranchKind::Float64Truthy` (ноль и NaN ложны).

## Fixed-work (весь cohort)

CLI `2a02e5179e02a615ba228aebda42e565359756cab3fe7bd6a1586ec0a0d084a5`.

| Бенч | Otter/Node | Δ к c421afbcb |
|---|---:|---:|
| richards | 2.459987x | -0.005041% |
| deltablue | 5.309890x | -0.240424% |
| crypto | 1.594956x | -0.016038% |
| raytrace | 2.548797x | **-7.841828%** |
| earley-boyer | 1.584165x | +0.014704% |
| regexp | 6.601310x | +0.014196% |
| splay | 2.097482x | -0.223887% |
| navier-stokes | 1.097973x | -0.012527% |
| box2d | 2.663648x | -0.232612% |
| gbemu | 3.174296x | -0.014340% |
| mandreel | 1.375137x | -0.014669% |
| zlib | 0.983360x | -0.010269% |
| ts | 1.789887x | -0.191255% |

Геометрическое среднее **2.202282621139→2.186921238278, −0.697521%**.
Регрессии: earley-boyer +0.015%, regexp +0.014% (шум одиночного прогона).

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-12c`. Difftest host/x86 153/0 (новый
`store_shape_truthiness.js`: −0/NaN/int32 truthiness, алиас при transition,
полиморфный store-сайт). VM 1245/0, JIT 270/0, x86 293/0, clippy host/x86.
GC stress 256 всех 13: вывод совпал. Корпуса совпали с Node во всех режимах.
