# Frame binding-cell batches

Accepted on top of `7e0bd47b`, macOS ARM64. Validated and measured below.

## Measured mechanism

The post-inline-closure Earley profile has 1790 isolate self samples: local
binding-cell allocation 94, generated upvalue initialization 54, and shared
old-space allocation 75. The last category is shared with other object types.
The allocation census counts 110,900,416 binding cells (1,774,406,656 bytes).
The rejected nursery-policy experiment adds 6.27% instructions. Moving the same
number of cells to young generation is not the selected mechanism.

[BEAM AArch64 allocation checks](https://github.com/erlang/otp/blob/master/erts/emulator/beam/jit/arm/instr_common.cpp),
`emit_allocate_heap` / `emit_gc_test`, reserve the known heap/stack requirement
at one rooted boundary before the instruction's allocations. Otter applies the
reservation principle to the known own-binding count of a frame. It retains JS
mutable binding identity, individual object headers and normal tracing. The
source snapshot and hash are `sources/beam-allocation-reservation.{cpp,json}`.

## Invariants and integration

The heap batch primitive checks the entire allocation budget, rooting the
pending repeated payload and caller inputs. It then reserves page-bounded
ranges and fills them with independent ordinary GC objects without another
safepoint. Each object has its own header, black allocation state, outgoing
barriers and per-type allocation count. A single captured cell does not retain
its unused neighbors. Large batches span old-space pages rather than pretending
to be one large-object-space cell.

All interpreted frame builders and generated native entry use this primitive.
Generated frames publish zero own upvalues at the allocation boundary, then
publish the completed prefix before copying inherited handles from the rooted
closure. Both native backends enter through this same existing initializer.
Independent single-cell creation remains appropriate for loop renewal, derived
constructor state and permanent global lexicals; no alternate frame-building
loop or mode remains.

The preserved baseline CLI is `otter-before-young-bindings`, SHA-256
`8885cc248dd3de844554a68be0a1bd03971436d22284ee44bc1bb5f07127c10b`.
It contains the accepted inline closure representation and old binding policy.
Raw artifacts live under ignored `benchmarks/results/parity-2026-09-27/`.

## Validation and measurements

Executable SHA-256 `78464bd7db1fdcf54d564d56eac8f746f10247ecac277316098643a6c65c371d`
(`benchmarks/results/arch-2026-09-27/otter-wip-cell-batches`). Reference is the
preserved `otter-before-young-bindings` (HEAD code, SHA-256 `8885cc24…`).

Gates: differential **59/59**; JIT library **283/283**; batch collector tests
**4/4** (cross-page independence, pending value and edges as moving roots,
cap refusal, black allocation during marking). GC stress strides 1..16 in
interpreter, Template and production with `OTTER_GC_VERIFY=1`, Node stdout
agreement: **384/384** over `captured_environment_aliases`,
`arguments_local_reads`, `closures`, `inline_closure_upvalue_reads`,
`machine_make_closure`, `generator-bootstrap`, `allocation_gc`,
`calls_recursion` (`scripts/dev/gc-stress.py`, raw `wip-stress/`).

Earley alternating pairs (candidate/reference, reference/candidate):

| Pair | Reference instructions | Candidate instructions | Change | RSS reference | RSS candidate |
| --- | ---: | ---: | ---: | ---: | ---: |
| 1 | 229,143,739,041 | 215,494,776,811 | −5.96% | 134,512,640 | 133,873,664 |
| 2 | 229,098,685,333 | 215,484,843,609 | −5.94% | 133,840,896 | 133,595,136 |

All seven fixed-work workloads, sequential candidate then reference
(`wip-final/`, `wip-reference/`):

| Workload | Reference | Candidate | Change | RSS ref MB | RSS cand MB |
| --- | ---: | ---: | ---: | ---: | ---: |
| ts | 198,190,110,624 | 198,395,861,555 | +0.10% | 668.5 | 545.6 |
| zlib | 273,333,953,280 | 273,337,187,081 | +0.00% | 783.2 | 799.5 |
| crypto | 16,830,486,250 | 16,843,117,287 | +0.08% | 48.3 | 48.0 |
| fib | 3,814,530,128 | 3,814,263,681 | −0.01% | 56.4 | 56.3 |
| mega_method | 10,122,132,431 | 10,124,191,980 | +0.02% | 56.6 | 56.9 |
| ast_ctor | 31,730,591,298 | 31,736,829,300 | +0.02% | 123.9 | 124.1 |
| earley-boyer | 229,066,646,950 | 215,431,761,027 | −5.95% | 127.9 | 127.6 |

Only Earley changes beyond noise. TS/zlib RSS varies run to run by tens of
megabytes in both directions (see earlier reports); no memory claim is made.
The 110.9M binding cells remain; this change only amortizes their allocation
protocol. The environment representation itself is the next measured target
(`2026-09-27-architecture.md`).
