# Rejected young-binding allocation experiment

Parent revision `76aa818d406e0d37c4f54c1f22c05572025ef333` (signature G).
The inline-closure change remains. This experiment was removed completely.

## Evidence and hypothesis

The fresh post-closure Earley profile has 1790 isolate self samples, including
94 in alloc_upvalue_with_roots, 54 in generated upvalue initialization and 75 in
the shared old-space allocator. Sweep and mark-start have 96 and 86 additional
self samples; their costs are shared and cannot all be assigned to bindings.
The allocation census still counts 110,900,416 binding cells.

Current [V8 factory.cc](https://github.com/v8/v8/blob/main/src/heap/factory.cc)
allocates function contexts in young generation and script/module contexts in
old generation. The source snapshot and hash are retained in
`sources/v8-factory-young-context.{cc,json}`. This supports testing a generation
policy, not assuming its benefit for Otter's separate cells and old closures.

The pilot changed local binding allocation to the existing generational
allocator. Global lexical cells retained an explicit old-space allocator because
generated global proofs embed their stable addresses. Cell size, sharing and
compiled frame-slot stride did not change.

## Measurement and rejection

All files below are under ignored `benchmarks/results/parity-2026-09-27/`.
The preserved binaries are `otter-before-young-bindings` and
`otter-young-bindings-pilot`. Their SHA-256 hashes, exact commands and counters
are in `young-bindings-repeat.json`. Both execute the unchanged Earley source
with `/usr/bin/time -l`; stdout matches.

| Executable | Instructions | Peak RSS bytes |
| --- | ---: | ---: |
| Before | 230,507,186,659 | 135,086,080 |
| Young binding pilot | 244,967,002,124 | 135,430,144 |

Instructions regress **6.273%**, RSS **0.255%**. An earlier pilot also retired
244,900,696,500 instructions, RSS 135,610,368 bytes. This is a repeatable
instruction regression and does not justify production acceptance. The pilot
was stopped before all-seven measurement and both gates; it is not a validated
production implementation. No full-suite performance claim is made for it.

## Root audit and next step

An audit also found copied mapped-argument cells and copied inherited frame
cells whose lifetimes rely on the existing old allocation. A proposed root
repair was saved in `rejected-young-bindings-with-root-audit.patch` with the
experiment, then removed. **That wider patch was not the measured executable.**
The pilot executable contains only the allocation-policy and global-allocation
changes; it is unsafe to publish it as a completed moving-binding design.
`young-bindings-audit.md` records the consumers requiring attention.

The source tree and `target/release/otter` were restored to the accepted inline
closure implementation. Do not repeat this policy-only experiment as an
optimization. The next hypothesis is eliminating unnecessary mutable cells for
proven immutable captures while preserving four-byte capture entries, shared
mutable identities, initialization order and exact GC slot rewriting. This
requires a compiler proof and a complete runtime/compiled representation change.
