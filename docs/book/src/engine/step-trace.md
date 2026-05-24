# Step Trace

Otter ships a per-instruction execution trace. Every dispatched
opcode produces one canonical line of text. Embedders install the
tracer once; the dispatch loop pays a single `Option` discriminant
check per instruction when no tracer is installed.

## CLI

```sh
otter --trace=- run path/to/script.ts
otter --trace=trace.log run path/to/script.ts
otter --trace=- path/to/script.ts        # shorthand form
```

`--trace` writes to stderr; `--trace=<path>` opens the file
truncating any existing contents. The flag is available on every
subcommand because the trace gate lives on the runtime; the same
flag works for `run`, the positional shorthand, and `eval` /
`-e` / `-p`.

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

## Performance

When no tracer is installed the dispatch loop executes a single
`Option::is_some` branch per instruction. Branch prediction keeps
the off-path effectively free; benchmark suites under
`crates/otter-vm/benches/` are unchanged.

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
