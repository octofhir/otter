# Allocation-site pretenuring of object and array literals

Parent `6ff3d696c26d64c2c0c8b332a2a9bbc73ce7f176`. Machine provenance:
[gate-summary.json](gate-summary.json), [performance.json](performance.json).

## Обязательная работа, которая исчезла

Splay до среза: 44 scavenges, nursery input 1376 MiB, **97.5%** выживает и
сразу уходит в old (1309 MiB). Каждый такой объект выделялся в nursery,
затем копировался, получал old-space cell и трассировался scavenger'ом.
После среза те же литералы рождаются в old: nursery input 484 MiB,
16 scavenges.

## Архитектура

- `otter-gc::pretenuring`: site record (created/found/decision) по
  стабильному адресу; digest после каждого scavenge: ≥100 created, ratio ≥0.85
  и nursery на максимуме → `Tenure`; ниже → `DontTenure`; решения финальны.
- Site id живёт в 2 header bytes young-клетки; scavenger считает найденную
  клетку один раз и очищает тег у копии.
- Old machine LAB рядом с young: whole-chunk accounting, filler при sync,
  возврат хвоста в page/free list/cap при retire; retire на каждой
  collection boundary, refill только когда machine allocation разрешена.
- VM: site id в typed feedback slot `NewObjectLiteral`/`NewArray`; runtime
  аллоцирует tagged young или old (`SitePlacement`).
- JIT: план site (id, record, mode). `Track`: tagged header word, `created++`,
  проверка финальности → eager `AllocationSiteDecided` → recompile. `Old`:
  fit в old LAB, `WriteBarrier` на cell-входы object literal; массив в old
  только numeric, tagged — committed path.

## Fixed-work (весь cohort)

CLI `e1f7951bfacdb2c000693d19fbb6538482231a2a2ea4bf2a88e7099c78ee4db5`.

| Бенч | Otter/Node | Δ к 6ff3d696c |
|---|---:|---:|
| richards | 2.460813x | +0.003225% |
| deltablue | 6.820487x | +0.032018% |
| crypto | 1.596218x | +0.089959% |
| raytrace | 4.138379x | -0.036709% |
| earley-boyer | 1.585450x | +0.091185% |
| regexp | 6.602611x | -0.162870% |
| splay | 2.101430x | **-24.390083%** |
| navier-stokes | 1.098345x | +0.041412% |
| box2d | 2.710361x | +0.068966% |
| gbemu | 3.174684x | +0.205592% |
| mandreel | 1.375686x | +0.072259% |
| zlib | 0.983598x | +0.020082% |
| ts | 1.797685x | -0.135957% |

Геометрическое среднее **2.368806510821→2.318920696598, −2.105947%**.
Регрессии: gbemu +0.206%, earley-boyer +0.091%, crypto +0.090%, mandreel
+0.072%, box2d +0.069%, navier-stokes +0.041%, deltablue +0.032%, zlib
+0.020%, richards +0.003%.

## Correctness

test262: 53 575 / 52 463 / 12 fail / 1100 skip, 0 timeout/OOM/crash, failure
set идентичен `full-2026-10-11h`. Difftest host/x86 150/0 (новый
`allocation_site_pretenuring.js`). GC 84/0, VM 1245/0, JIT 270/0, x86 293/0,
runtime 12/0, clippy host/x86. GC stress 256 всех 13: вывод совпал. Корпуса
совпали с Node во всех режимах: 21/21.
