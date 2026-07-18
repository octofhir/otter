---
title: "JIT Debugging"
---

Otter can capture two independent, default-off JIT diagnostic channels:

- structured events explain when and why compilation, inlining, OSR, side
  exits, and deoptimization happened;
- artifact bundles preserve the compiler input, exact native output, symbolic
  address sites, and a portable comparison stream for each successful compile.

Neither channel writes from the VM or JIT compiler. The engine returns bounded,
owned data to the outer runtime, and the CLI performs filesystem I/O only when
the corresponding flag is present.

## Capture a run

Build the release CLI, then run a reproducible tier:

```sh
cargo build --release -p otter-cli

target/release/otter \
  --jit-tier=template \
  --jit-events=jit-events.json \
  --jit-artifacts=jit-artifacts \
  run examples/jit_bench.js
```

Use `--jit-tier=template` to isolate template lowering,
`--jit-tier=production-tiered` to include the optimizing tier, and
`--jit-tier=interpreter` as the no-native-code oracle. A diagnostics target is
never implied by the tier.

`--jit-events` without a value defaults to `otter-jit-events.json`.
`--jit-artifacts` without a value defaults to `otter-jit-artifacts`. Both flags
also accept an explicit value with `=`. The artifact target must name a
directory that does not exist. Under the cooperative single-writer contract,
Otter writes a private sibling and renames the complete root into place. This
is atomic visibility, not crash-durable storage or cross-process locking.

On a JavaScript exception, Otter keeps the original runtime error primary and
best-effort persists every successful compile already captured. A host timeout
that fires before the isolate replies may have no partial batch to write.

## Directory layout

The root contains a versioned index plus one directory per retained successful
compile:

```text
jit-artifacts/
  index.json
  jit-0000-template-f7-c11/
    manifest.json
    bytecode.txt
    template-plan.txt
    code.bin
    code-normalized.bin
    asm.txt
    code-map.json
    relocations.json
    safepoints.json
  jit-0001-optimizing-f9-c12/
    manifest.json
    bytecode.txt
    optimized-ir.txt
    code.bin
    code-normalized.bin
    asm.txt
    code-map.json
    relocations.json
    deopt.json
    safepoints.json
```

The suffixes identify capture order, tier, VM function id, and isolate-local
code-object id. `index.json` also reports retained bytes, dropped bundles,
dropped bytes, and whether the hard count or byte bound truncated the capture.

Every `manifest.json` uses `otterJitArtifactSchemaVersion`. It records the Rust
target triple, architecture, operating system, tier, function and module
identity, entry kind, bytecode size, code size, and explicit `filesPresent` /
`filesAbsent` inventories.

## Payloads

| File | Meaning |
| --- | --- |
| `bytecode.txt` | Deterministic logical-PC and encoded-byte-PC listing. |
| `template-plan.txt` | The already-built template lowering plan and its decoded operand side buffers. |
| `optimized-ir.txt` | The already-built optimizing unit in deterministic reverse-postorder. |
| `code.bin` | Exact finalized executable bytes for this runtime process. |
| `code-normalized.bin` | Non-executable semantic instruction stream with symbolic relocations and logical branch targets. |
| `asm.txt` | Versioned annotated AArch64 assembly over the exact bytes in `code.bin`. |
| `code-map.json` | Native offset ranges correlated with bytecode/tier operations, structural regions, and OSR entries. |
| `relocations.json` | Typed runtime-local address sites and their exact `code.bin` ranges, without resolved address values. |
| `deopt.json` | Optimizer frame reconstruction metadata; omitted for template code. |
| `safepoints.json` | Tagged frame/register/spill locations known to moving GC. |

`code.bin` is intentionally marked runtime-local. It can contain baked process
addresses and can differ across identical runs because of ASLR. Do not use it
as a portable golden file.

All native locations are offsets into the matching `code.bin`, never absolute
executable addresses. A range must satisfy
`0 <= startOffset <= endOffset <= manifest.codeBytes`.

## Annotated ARM64 assembly

`asm.txt` is ordinary UTF-8 assembly text with a versioned two-line header:

```text
; otter jit aarch64 assembly v1
; offset-basis=code.bin
```

The banner is the format/schema discriminator. Every rendered instruction or
relocation range starts with `+0x<8-hex>:`, measured from byte zero of the
sibling `code.bin`. A relocation range is deliberately rendered as one
symbolic line; intermediate MOV-wide instruction offsets stay hidden with
their address chunks. Local branch destinations are rendered as
`L<8-hex-offset>` labels, so a branch can be followed without exposing a
process address. If the built-in decoder does not recognize a four-byte
instruction, the exact word remains visible as `.word 0x<8-hex>`; one unknown
instruction therefore does not make the remainder of the artifact unavailable.

The remaining header comments record target, architecture, operating system,
tier, function/module/code-object identity, compile target, entry offset,
exact code length, deopt summaries, and the safepoint inventory. Body lines
use these stable forms:

```text
  ; region kind=<kind> range=+0x<start>..+0x<end> ... pc=<pc> byte-pc=<byte-pc> tier-op="<operation>"
L<8-hex-offset>:
+0x<8-hex>: <8-hex-word>  <decoded instruction or .word fallback>
+0x<8-hex>: relocation <register>, <symbolic target> ; encoded-bytes=<n> redacted
```

Comments are derived from metadata the compiler already owns. They identify
the overlapping `code-map.json` region and, when applicable, its function,
logical/encoded bytecode PC, template operation or optimized-IR operation,
structural block/edge, OSR entry, or deopt exit. A baked address load is
replaced by one offset-bearing `relocation …` pseudo-line with the typed
symbolic target from `relocations.json`; its resolved pointer and immediate
chunks are deliberately redacted. Instruction annotations use stable `pc=`
and `tier-op=` fields for bytecode/tier correlation. Use `code.bin` when exact
executable bytes matter and `code-normalized.bin` for portable cross-process
comparisons.

All joins use the same `code.bin` offset basis:

- `code-map.json` maps assembly offsets and ranges back to bytecode and tier
  operations;
- `relocations.json` describes the symbolic meaning of baked-address ranges;
- `deopt.json` supplies frame reconstruction for a `deoptExitId` named by the
  code map or assembly annotation;
- `safepoints.json` supplies tagged frame/register/spill locations by
  safepoint id and frame state.

Safepoint records currently serialize `nativeReturnOffset: null`. Do not infer
an exact call-return instruction from assembly proximity; direct
safepoint-to-native-return correlation remains a follow-up. The corresponding
assembly summary says `native-offset=unavailable` while still preserving the
safepoint id, frame state, and tagged-location inventory.

Assembly generation is part of explicit artifact capture. Without
`--jit-artifacts`, compilation does not clone finalized code for diagnostics,
run the decoder, format assembly, or perform artifact filesystem I/O.

## Portable code comparisons

`relocations.json` uses `otterJitRelocationSchemaVersion: 1` and
`offsetBasis: "code.bin"`. Each sorted record describes the exact
`MOVZ`/`MOVK` range, destination register, emitted chunk shape, and a typed
symbolic target such as a runtime-stub descriptor, call trampoline, GC cage
base, property IC cell, or code-owned operand slice. Chunk immediates and
resolved pointer values are deliberately absent.

`code-normalized.bin` starts with the `OTJNCODE` schema marker. It is a
semantic comparison stream, not ARM64 executable code:

- a one-to-four instruction address load becomes one symbolic relocation
  token;
- local branch displacements become logical item targets, so ASLR-driven
  changes in address-load length do not move the comparison target;
- ordinary non-PC-relative instructions retain their exact instruction word.

Match bundles by module, function, tier, and entry, then compare their
normalized streams. Continue to use `code.bin` offsets when joining the code
map, relocation records, OSR entries, deopt exits, or safepoints.

## Correlate an execution

Use this order when a hot function produces a wrong result or unexpected
fallback:

1. Re-run with `--jit-tier=interpreter` to establish the bytecode oracle.
2. Capture `--jit-events` and find the function's `compilePrepared`,
   `compileFinished`, OSR, bail, or deopt records.
3. Join a successful `compileFinished` to `manifest.json` by `codeObjectId`.
4. Read `bytecode.txt` and the tier input to identify the logical operation.
5. Use `code-map.json` to map its logical PC and encoded byte PC to the exact
   native byte range.
6. Open `asm.txt` at the matching `+0x<8-hex>:` offset to inspect the emitted
   instructions and local branch labels.
7. Inspect `relocations.json` when the range materializes a runtime-local
   address.
8. Inspect `deopt.json` and `safepoints.json` when the range crosses a deopt or
   allocation boundary.

The interpreter [step trace](/otter/engine/step-trace/) complements this
capture: it shows the warmup and the last interpreter-visible PC, while the
artifact bundle explains the native body entered after that point.

## Embedding

Embedders request either channel explicitly:

```rust
use otter_runtime::{JitDebugRequest, Runtime, SourceInput};

let request = JitDebugRequest::disabled()
    .with_events(true)
    .with_artifacts(true);
let mut runtime = Runtime::builder()
    .jit_debug(request)
    .build()?;

let mut result = runtime.run_script(
    SourceInput::from_javascript("function hot() { return 42; } hot();"),
    "main.js",
)?;
let events = result.take_jit_debug_report();
let artifacts = result.take_jit_artifacts();
```

For abrupt completion, use `run_script_with_diagnostics` and inspect
`ExecutionAttempt::jit_debug_report()` plus
`ExecutionAttempt::jit_artifacts()`. Returned reports and bundles own all
strings and bytes; they contain no GC handle, executable pointer, isolate
borrow, lock, TLS state, or runtime registry reference, so they remain valid
after full GC and later JIT compilation.
