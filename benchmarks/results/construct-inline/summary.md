# Constructors that forward to an inherited initializer

Parent `9fe08094d`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

Raytrace строит классы через `Class.create()`: каждый конструктор —
`function () { this.initialize.apply(this, arguments); }`, одно тело на все
классы. Конструктор не встраивался (тело форвардит `arguments`), поэтому каждый
`new Vector(...)` — отдельный кадр c7: мегаморфный `this.initialize` через
shared table, `apply`, generic вызов `initialize`, затем stores через
программы в отдельном кадре `initialize`. c7 + `initialize` ≈ 53% сэмплов.

## Архитектура

- VM: тело, которое форвардит `arguments`, встраивается
  (`splices_without_argument_frame` запрещает только rest и
  `CollectArguments`); `forwards_arguments()` на CodeBlock.
- Builder: `CallForwardArguments` в inline-теле — вызов цели с
  аргументами вызова, mapped formals подставляют текущие значения; цель
  может встроиться дальше. Inline-кадр записывает actuals для deopt.
- VM: inline-тело конструктора для `new`-сайта с планом receiver получает
  `direct_methods` для named load/method call сайтов без собственной
  feedback: ключ, который receiver начальной формы наследует как closure
  (`resolve_inherited_atom_data_slot` от прототипа формы), с обычным
  method guard (форма, chain proof, holder slot). Резолв — до первой
  аллокации bake.
- Builder: такой load при доказанной форме receiver — `LoadGuardedMethod`;
  форвард/вызов прочитанного метода берёт его plan и тело.

## Fixed-work (весь cohort)

CLI `f204b570954bb690d543044a32587324fc209b52dfabb8d072b8ba5104d6c73e`.

| Бенч | Otter/Node | Δ к 9fe08094d |
|---|---:|---:|
| richards | 2.460111x | -0.026499% |
| deltablue | 5.322687x | -0.049311% |
| crypto | 1.595212x | +0.006115% |
| raytrace | 2.765677x | **-33.163857%** |
| earley-boyer | 1.583933x | -0.035438% |
| regexp | 6.600373x | -0.033843% |
| splay | 2.102188x | -0.036671% |
| navier-stokes | 1.098110x | -0.043000% |
| box2d | 2.669858x | -1.331266% |
| gbemu | 3.174751x | -0.001748% |
| mandreel | 1.375338x | -0.023374% |
| zlib | 0.983461x | -0.017777% |
| ts | 1.793317x | +0.033482% |

Геометрическое среднее **2.274351833994→2.202282621139, −3.168780%**.
Регрессии: ts +0.033%, crypto +0.006% (шум одиночного прогона).

## Correctness

Attempt 1 остановился на clippy: два binding-теста вызывали
`bake_inline_body` со старой сигнатурой; исправлено, freeze обновлён,
attempt 2 прошёл весь гейт. test262: 53 575 / 52 463 / 12 fail / 1100 skip,
0 timeout/OOM/crash, failure set идентичен `full-2026-10-12b`. Difftest
host/x86 152/0 (новый `construct_forward_inline.js`: подмена `initialize`,
own-ключ до конструктора, mapped/strict formals, исключение из цели). VM
1245/0, JIT 270/0, x86 293/0, clippy host/x86. GC stress 256 всех 13: вывод
совпал. Корпуса совпали с Node во всех режимах.
