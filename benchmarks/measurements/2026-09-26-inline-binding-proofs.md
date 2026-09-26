# Global reads in inlined bodies went through the runtime (2026-09-26)

Machine: Apple M1, release. Binaries: `68870652` (before) vs the change.
Spotlight indexing and ANECompilerService occupied ~2 cores during the
measurements (load average 3–4); both binaries ran under the same load,
interleaved per suite.

## Triage that found it

`scripts/dev/octane-triage.sh` over all 15 suites: the optimizing tier made
seven suites *slower* than the template tier alone (deltablue 593 vs 1435
jitless, box2d 2282 vs 3518, gbemu 895 vs 1372, richards 1156 vs 1386, …)
with almost no deopts and ~70 ms of total compile time. The new
`scripts/dev/octaneprof.py` attributes isolate samples to generated code,
the runtime boundary it calls, and native-only stacks:

```
deltablue tiered: jit leaf 16.9%, jit->native 83.1%
  61.5%  jit_binding_value_stub
  12.1%  jit_call_method_value_stub
   3.8%  jit_load_property_stub
```

Inside the binding stub the cost was `inline_frames::decode` (heap
reallocation per call) — the global load itself was a few samples.

## Cause

`bake_inline_body` baked a spliced callee's constants, string cells, global
lexical loads, property CacheIR and call plans, but not its
`binding_hit_proofs`. Every global read inside an inlined body therefore
lowered to `BindingGuard { target: Cold }`: an unconditional runtime call
that also decoded and published the inline activation chain. The root
function's own global reads were proven; its inlined callees' were not, so
inlining — the optimizing tier's main win — turned every global reference in
a callee (class names, constants, helper functions) into a runtime call.

## Change

`bake_inline_body` bakes the body's binding proofs like any outermost
function. The proofs are self-validating (global-lexical cell identity, or
global-object shape plus declarative epoch), so a later redefinition misses
into the same cold boundary as before.

## Result (tiered, median of 3)

| suite | before | after | |
|---|---:|---:|---:|
| richards | 1133 | 3075 | 2.71x |
| deltablue | 620 | 1575 | 2.54x |
| splay | 2591 | 2781 | 1.07x |
| crypto | 312 | 307 | |
| raytrace | 1921 | 1967 | |
| earley-boyer | 2059 | 2063 | |
| regexp | 176 | 176 | |
| navier-stokes | 4897 | 4913 | |
| pdfjs | 685 | 683 | |
| mandreel | 831 | 836 | |
| gbemu | 899 | 902 | |
| code-load | 2963 | 3025 | |
| box2d | 2305 | 2315 | |
| zlib | 864 | 864 | |
| typescript | 762 | 757 | |

Lines: +1 (compiler input), test assertion +9.

## Open

- `inline_frames::decode` allocates on every runtime call from an inlined
  region (`jit_call_method_value_stub` is still 12% of deltablue). V8
  reconstructs inlined frames lazily from deopt data only when a stack walk
  or deopt needs them.
- crypto: ordinary objects have no indexed elements store; `BigInteger`'s
  `this[i]` digits are string-keyed shape properties.
