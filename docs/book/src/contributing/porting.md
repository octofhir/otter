# Porting Process

This process applies when moving behavior from parked compatibility crates,
reference implementations, or external sources into the active engine.

Use durable markers for uncertain migrations:

- `TODO(port): <reason>` when behavior is not fully understood yet.
- `PERF(port): <original invariant> - profile before optimizing` when replacing
  a known hot-path trick with idiomatic Rust.
- `PORT NOTE: <why shape changed>` when GC rooting, scheduling, ownership, or
  borrow-checker constraints require a control-flow change.

Do not use `todo!()` or `unimplemented!()` in reachable runtime code.

Keep JS-visible ports as vertical slices. Update runtime behavior, TypeScript
declarations, docs, examples, and targeted tests together when the surface is
observable by users.

Preserve active-stack boundaries:

- VM/runtime work belongs in `crates-next/otter-gc`, `crates-next/otter-vm`, and
  `crates-next/otter-runtime`.
- Standards-facing Web APIs should move into the active Web API crate when that
  crate is revived.
- Otter-hosted modules should move from `crates/otter-modules` into active-stack
  product crates before they are linked into the current runtime.
- Parked compatibility crates are reference code only; do not add active
  path-dependencies on legacy `crates/*`.

For large ports, add a short status block when it helps review:

```rust
// PORT STATUS
//   source:     <parked crate/file or reference area>
//   confidence: high | medium | low
//   todos:      N
//   tests:      <targeted tests or reason omitted>
//   notes:      <one line for reviewers>
```
