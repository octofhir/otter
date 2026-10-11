# Call/frame contract

Parent: `5186cffedb52cd5b37986cb13f65c570095b8563`.

## Contract

Both Graph encoders use one admission proof: an actual-only, nonderived body with no emitted collecting, reentrant, throwing, lazy-deopt or loop-poll boundary may reserve its machine extent without publishing a Rust Frame. Argument-window/cache consumers are excluded. Incoming values and the staged caller anchor remain owned by the reserved extent. Return-word bit one marks its unpublished state; aligned caller pointers and constructor bit zero remain unambiguous.

At underarity, receiver preparation, construction or eager reconstruction, the encoder completes and publishes the same VM-owned Frame before the first runtime access. Canonical homes use the stable native frame pointer even under a register dump. A successful ordinary call restores only machine registers. Tier/OSR entries retain prior root-word restoration. No new carrier, version, adapter, generic call path, environment flag or mutable process state.

Fixed pre-existing abrupt receiver-preparation bug: a nonconstructor Throw/Fatal path must not read inactive constructor layout/super-origin tickets. Cold guard and poisoned-ticket x86 fixtures cover both completion domains.

## Evidence

Deltablue current-binary self profile: 2350 → 2348 samples; generated 1557 → 1595. Entry 179 → 167; ordinary return 51 → 43. Entry/return share 14.77% → 13.17% of generated samples. Samples suggest direction but are not retired instruction counts. Only self rows and exact code intervals are used; no inclusive double counting.

Tiny noninlined f579: frame prologue 33 → 22 emitted instructions, plus one hot receiver-proof instruction (34 → 23). Ordinary called return retires 11 → 8 instructions. Root/scratch, source-owner, empty-window and chain-publication work disappears from its hot entry. The cold completion code remains available.

Fresh before fixed work: deltablue 40.34 G; final 39.91 G. Full table uses exact field-tracking gate counts as the common baseline. No inlining threshold increase is needed to earn this result; future admission must price the reduced boundary against retained proof/reconstruction costs.

## Fixed work

| Benchmark | Otter G | Otter/Node | Instructions delta |
|---|---:|---:|---:|
| richards | 28.5058 | 2.4659x | -0.8102% |
| deltablue | 39.9064 | 7.1377x | -0.9799% |
| crypto | 4.6759 | 1.6023x | -0.1832% |
| raytrace | 34.6477 | 4.1388x | -1.2748% |
| earley-boyer | 29.6880 | 1.5886x | +0.0015% |
| regexp | 51.0433 | 6.9903x | -0.1764% |
| splay | 12.1888 | 2.7723x | -0.2310% |
| navier-stokes | 8.8852 | 1.0976x | -0.0746% |
| box2d | 14.1914 | 2.8032x | -0.4583% |
| gbemu | 26.3201 | 3.2015x | -0.6985% |
| mandreel | 19.1877 | 1.3881x | -0.1111% |
| zlib | 24.5015 | 0.9900x | -0.0250% |
| ts | 37.8055 | 1.8128x | -0.3530% |

Geomean: 2.4111x → 2.4011x. The sole positive delta is earley-boyer (+0.001%); all other benchmarks decrease. The older Splay field-observation regression is reduced only slightly and remains separate work.

## Validation

Full gate passed: Test262 52,463 pass / the same 12 fail / 1,100 skip, no timeouts, crashes or OOM. Interpreter-oracle difftest 139/0 on ARM64 and x86. VM 1223/0; host JIT 248/0; extra x86 JIT 270/0. Both target clippy checks passed with -D warnings. GC stress 256 passed all 13 benchmarks with matching output. Interpreter/Node/native/stress-256 corpus outputs match. Source and measured binary fingerprints are frozen in gate-summary.json.

The extra x86 unit gate exposed four pre-existing stale property-action fixture offsets; fixtures now use the authoritative DTO fields. This and the missing test shape fields were cfg(test)-only repairs; production binary hashes remained unchanged.
