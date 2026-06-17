# Engine perf profiles

Profiling artifacts behind [`../../ENGINE_PERF_AUDIT.md`](../../ENGINE_PERF_AUDIT.md).

- `*.json.gz` — samply (Firefox processed-profile) captures, one per
  `<bench>.<on|off>` where `on` = JIT (default), `off` = `OTTER_JIT=0`.
  Open at <https://profiler.firefox.com> or `samply load <file>`.
- `*.selftime.txt` — busiest-thread, lib-classified, atos-symbolicated
  self-time top-25 for each profile.

## Scripts

| script | does |
|---|---|
| `collect.mjs` | runs every bench JIT on+off, captures `durationMs` + `OTTER_STATS=1` counters → markdown table (`otter-stats.md`) |
| `prof.sh` | `samply record --save-only` a list of benches (on+off) and print self-time |
| `symbolicate.mjs` | parse one profile, classify leaves by lib, atos `otter` frames against the dSYM, print self-time by function |
| `parse.mjs` | minimal unsymbolicated leaf histogram (debug aid) |

## Prereqs

```bash
cargo build --release -p otter-cli      # release profile carries debug=1
dsymutil target/release/otter           # dSYM for atos symbolication
```

Re-run everything:

```bash
node benchmarks/profiles/collect.mjs > benchmarks/profiles/otter-stats.md
bash benchmarks/profiles/prof.sh sort.js json.js prop-access.js nbody.js
```
