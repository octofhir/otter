# Rejected experiment: shared field observation

Checkpoint: `fa4cc66682168136cbd96f86a0849e441b9f3335`. The prototype was removed from the active code. It is retained only as experimental evidence; the current architecture plan is in `2026-10-22-engine-architecture-audit.md`.

The prototype pooled the classifier and preserving C-call sequence once per native generation on both AArch64 and x86_64. The final variant retained a local scalar membership test. Cumulative captured native code fell from 30,089,596 to 25,022,068 bytes, but generation counts differed (1,410 versus 1,411); this is not a retained-memory measurement.

Five alternating process pairs, one excluded warmup pair, whole unchanged fixtures:

| Workload | Wall median before → after | CPU | Peak RSS |
|---|---|---|---|
| ts | 3.256 → 3.168 s (−2.70%) | 4.47 → 4.40 s | 575.81 → 572.19 MiB |
| splay | 0.712 → 0.696 s (−2.26%) | 1.27 → 1.29 s | 378.86 → 393.08 MiB |

Host variability was substantial. The TS paired changes ranged from −11.39% to +12.37%. These observations do not satisfy the requested substantial speed and memory improvement. An earlier inline-widening variant also failed that criterion. No successful post-GC retention comparison was obtained for these prototypes; emitted bytes must not be substituted for it.

Preserved binaries, exact patches, commands, hashes, stdout and raw measurements: `benchmarks/results/metadata-ownership-2026-10-21/compact-experiment/` and `scalar-inline-ab/`. The final removal patch is also in `benchmarks/results/architecture-census-2026-10-22/rejected-observer.patch`. Targeted observer tests passed on both ISAs, with x86_64 executed under Rosetta. Performance captures are AArch64 only.
