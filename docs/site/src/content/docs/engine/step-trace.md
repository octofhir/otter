---
title: "Step Trace"
---

Otter ships a per-instruction interpreter execution trace. Every opcode
dispatched by the bytecode interpreter produces one canonical line of text.
Embedders install the tracer once; each dispatch run probes for its presence
once and uses a hoisted boolean gate at each bytecode instruction.

## CLI

```sh
otter --trace=- run path/to/script.ts
otter --trace=trace.log run path/to/script.ts
otter --trace=- path/to/script.ts        # shorthand form
```

`--trace` writes to stderr; `--trace=<path>` opens the file truncating any
existing contents. The flag is global syntactically and affects execution
paths that construct the runtime: `run`, the positional shorthand, and
`eval` / `-e` / `-p`, plus files executed by `test`. Package-management and
information commands do not execute bytecode and therefore emit no step trace.

## Format

Each event renders as:

```text
frame=<depth> fn=<function-name> pc=<6-digit byte pc> op=<MNEMONIC>  <operand list>
```

- `frame` is the depth of the active call frame at dispatch time
  (1-based; depth 1 is the entry frame).
- `fn` is the source-declared name. Module entry is `<main>`.
- `pc` is the byte-offset PC inside the function's encoded stream.
- `op` is the mnemonic from
  [`otter_bytecode::Op::mnemonic`][op-mnemonic]; renaming an opcode
  changes the mnemonic in lock-step with the bytecode wire format.
- Operands follow the disassembler conventions: `r<N>` for
  registers, `k[<N>]` for constant-pool indices, `i32:<N>` for
  signed immediates.

The very first line of every trace is the version banner so
downstream consumers can hard-fail on format drift:

```text
; otter step trace v1
```

Bump [`otter_vm::inspect::TRACE_FORMAT_VERSION`][trace-version] on
any incompatible format change. Golden tests under
`crates/otter-runtime/tests/golden/` re-bless via the
`OTTER_BLESS_TRACES=1` environment variable.

## Embedding

Install a tracer through the runtime builder:

```rust
use otter_runtime::{Otter, TracerFactory};
use otter_runtime::inspect::{StepTracer, WriterTracer};

let factory = TracerFactory::new(|| -> Box<dyn StepTracer> {
    Box::new(WriterTracer::new(std::io::stderr()))
});

let otter = Otter::builder()
    .tracer_factory(Some(factory))
    .build()?;
```

Custom tracers implement [`otter_vm::inspect::StepTracer`][step-tracer]
directly — useful for in-process collectors, structured-logging
adapters, or debugger UIs. The factory runs once on the isolate
runner thread immediately after the interpreter is constructed.

## JIT Visibility

The step trace is an interpreter trace, not a native instruction trace.
Template and optimizing JIT bodies do not emit one event per native
instruction. A hot function may therefore show its interpreter warmup, then
an otherwise unexplained gap while its native body executes, until control
returns to the interpreter.

Use the trace to establish bytecode order and the last interpreter-visible
PC. To correlate a compiled function with its bytecode, tier input, exact
machine-code offsets, safepoints, or deopt exits, capture a
[JIT artifact bundle](/otter/engine/jit-debugging/). Its `asm.txt` is a static
annotated view of the exact `code.bin` bytes, with `code.bin`-relative offsets,
local branch labels, and redacted symbolic relocations. It fills the native
code-inspection gap but is not a live per-instruction execution trace.

## Performance

When no tracer is installed a dispatch run performs one hoisted tracer
presence probe and its loop executes one false boolean gate per interpreter
instruction. The tracer does not format events or call a sink on that path.
Its exact overhead must still be measured on the release benchmark baseline
rather than assumed.

When a tracer is installed the per-instruction work is one
`StepEvent` build (stack-only) plus one `on_step` virtual call.
[`WriterTracer`][writer-tracer] formats into a reusable string
buffer to avoid per-line allocation; embedders that need to skip
the formatting cost can implement `StepTracer` directly and treat
the event payload as structured input.

## Inspector Snapshots

The same `inspect` module exposes point-in-time DTOs that complement
the streaming trace:

- `Runtime::ic_snapshot()` returns one `IcSiteSnapshot` per
  property inline-cache site. Each site reports `Empty`,
  `Polymorphic { entries, misses }`, or `Megamorphic`. Polymorphic
  entries carry the receiver shape id, the matched slot offset, and
  the IC variant (`OwnData`, `DirectPrototypeData`,
  `OwnAddTransition`, …).
- `Runtime::shape_transition_snapshot()` returns the live
  hidden-class transition tree as a flat node list ordered by
  `(parent_shape_id, transition_key)`.
- `Runtime::set_shape_transition_observer(Some(obs))` installs a
  `ShapeTransitionObserver` that fires on every hidden-class
  transition the VM takes. The event carries the from / to shape ids,
  the key, and a `reused` flag that distinguishes fresh allocations
  from cached lookups — the right primitive for shape-transition
  breakpoints and shape-thrash audits.
- `FrameSnapshot::from_step_event(event, include_undefined)`
  builds a per-frame snapshot from inside a step tracer's
  `on_step`. Each `RegisterSnapshot` carries the register index and
  a compact debug repr of the live `Value`.
- `Runtime::heap_snapshot_summary()` returns a
  `HeapSnapshotSummary` — total live objects, total bytes, and one
  bucket per non-empty `Traceable::TYPE_TAG`. `render_text()` emits
  a deterministic table for diagnostic dumps.
- `Runtime::write_chrome_heap_snapshot(&mut writer)` streams a
  Chrome DevTools `.heapsnapshot` JSON document. The output loads
  directly into DevTools' "Memory" panel — no post-processing
  required.

The snapshot DTOs are plain owned structs (no GC handles). They are
safe to keep across mutator turns, log, or compare in tests.

## Schema Stability

The trace format is wedged to the bytecode wire format by the
schema tests in `crates/otter-vm/src/inspect.rs`:

- `every_table_op_has_unique_mnemonic` walks
  [`otter_bytecode::encoding::OP_BYTE_TABLE`][op-byte-table] —
  the authoritative dense byte table — and verifies every opcode
  produces a non-empty unique mnemonic.
- `op_table_matches_reference_list` cross-checks that the inspect
  module's reference list stays aligned with the wire-format
  table. Adding a new `Op` variant fails this test until the
  inspect reference list is updated, which is the gate that
  forces the golden traces to be revisited on every opcode
  change.

[op-mnemonic]: https://docs.rs/otter-bytecode/latest/otter_bytecode/enum.Op.html#method.mnemonic
[op-byte-table]: https://docs.rs/otter-bytecode/latest/otter_bytecode/encoding/constant.OP_BYTE_TABLE.html
[trace-version]: https://docs.rs/otter-vm/latest/otter_vm/inspect/constant.TRACE_FORMAT_VERSION.html
[step-tracer]: https://docs.rs/otter-vm/latest/otter_vm/inspect/trait.StepTracer.html
[writer-tracer]: https://docs.rs/otter-vm/latest/otter_vm/inspect/struct.WriterTracer.html
