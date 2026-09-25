# Mandreel hang and the lost `--timeout` (2026-09-26)

Machine: Apple M1, release build. Baseline HEAD `1b84d749`.

## Cause

`otter --timeout 300 run mandreel` spun for 25+ minutes of CPU. `sample`
showed 100% of the isolate thread inside `otter_compiler::expr::binary::compile_binary_to`
— the bytecode compiler never finished, so no JavaScript ever ran.

1. **Statement temporaries were never released.** Every statement kept the
   registers its expression used; a statement list cost the *sum* of its
   statements' windows. Mandreel's `global_init` has 458,985 bytecode
   instructions (~150k `emit_32(...)` statements) and exhausted the 65,535
   register window.
2. **Saturated allocation looped forever.** On overflow `alloc_scratch`
   returns `u16::MAX` on every call, and the binary-expression destination
   search `while candidate == lhs { candidate = alloc_scratch() }` never
   terminates once `lhs == u16::MAX`.
3. **The timeout was cooperative only.** The command reply timed out on time,
   but the final `RuntimeHandle` drop joined the isolate thread, which was
   stuck in Rust code that never polls the interrupt. The CLI printed the
   timeout only when the stuck work ended.
4. After (1)–(2) mandreel compiled, and the template tier then **aborted the
   process**: a ~1 MB function had a label reference past the ±1 MB branch
   range, and `dynasmrt::Assembler::finalize` panics on a relocation error
   (its `map_err` never runs).

## Change

- `compile_statement` releases every temporary when the statement ends,
  keeping only bindings the statement declared into the enclosing scope and —
  while a completion value is observable (script / eval) — the result register
  the enclosing form reads. Statement lists whose completion is discarded use
  `compile_discarded_statement`. The destination search stops once the window
  has overflowed, so an oversized function reports the existing
  `function body exhausts the 65535-register window` error.
- `finalize_assembler` commits relocations explicitly; an unencodable label
  is `BackendFailure::Relocation`, reported as an ordinary unsupported shape
  (no retry, no abort). Used by both tiers on both architectures.
- The final handle drop detaches — never joins — an isolate whose command
  timed out; the runner tears its `Runtime` down itself. The host (CLI, or an
  embedding UI thread) is never parked behind non-polling native code.

## Result

| Mode | before | after |
|---|---:|---:|
| tiered | hang (>25 min, killed) | 806 |
| `--jitless` | hang | 982 |
| `--interpreter` | hang | 284 |

node: 83596.

`--timeout 1` over a native `join`/`split` loop: 4.1 s → 1.05 s wall.

Register window of a 6-call function: 7 → 2.

Remaining mandreel gaps (next slices): tiered < jitless; Machine declines
`predecessor state merge` (8 functions) and `scalar HIR to Machine IR
selection` (11); one template function exceeds the ±1 MB `b.cond` range.
