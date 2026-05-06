# Task 97 Benchmark Report

Date: 2026-05-06

Command:

```bash
cargo bench -p otter-macros --bench js_surface_macros -- --sample-size 30 --measurement-time 2 --warm-up-time 1
```

Benchmark source:

- `crates-next/otter-macros/benches/js_surface_macros.rs`

The benchmark compares equivalent handwritten Task 96 static specs against
Task 97 macro-generated specs. The benchmark deliberately points handwritten
specs at the same Rust function symbols used by the generated specs, so the
comparison is only static-record shape plus builder install cost.

Criterion's `change` lines are relative to earlier local runs of the same
benchmark while this task was being iterated; they are not interpreted as a
Task 97 regression signal. The parity signal is the same-run handwritten vs
macro comparison below.

| Group | Handwritten | Macro-generated | Result |
|---|---:|---:|---|
| `js_surface_namespace_install` / `#[js_namespace]` | 130.47 us | 121.93 us | macro not slower |
| `js_surface_namespace_install` / `raft!` | 130.47 us | 119.56 us | macro not slower |
| `js_surface_class_install` / `#[js_class]` | 121.55 us | 120.55 us | within noise |

Conclusion: macro-generated JS surfaces use the same static spec and
`NativeCall::Static` builder path as handwritten specs. No macro-specific
runtime registry, metadata parsing, boxed closure dispatch, or per-call
allocation was measured.
