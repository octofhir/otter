# Engine Porting Process

This document defines process rules for Otter's Rust-native JS engine work: VM/runtime features, Web APIs, Node API
ports, hosted modules, FFI, GC/JIT work, and compatibility shim migration.

## Core Rules

### 1. Mark uncertainty explicitly

Use durable markers instead of silent guesses:

- `TODO(port): <reason>` when moving code from parked compatibility shims or an
  external reference and the behavior is not fully understood.
- `PERF(port): <original invariant> - profile before optimizing` when an
  idiomatic Rust implementation intentionally replaces a known hot-path trick.
- `PORT NOTE: <why shape changed>` when borrow-checker, GC rooting, scheduler,
  or ownership constraints require a control-flow reshape.

Rules:

- Do not use `todo!()` or `unimplemented!()` in reachable runtime code.
- Every marker must name the blocked invariant or follow-up test, not just say
  "fix later".
- Before merging a porting slice, grep the marker count and decide whether each
  marker is acceptable debt or must be resolved in the same patch.

### 2. Keep feature ports as vertical slices

For JS-visible behavior, avoid partial ports that only update Rust code. Update
the Otter triangle in one patch when applicable:

- Runtime behavior.
- TypeScript declarations in `crates/otter-pm/src/types/otter/`.
- Docs, examples, and targeted tests.

For ECMAScript features, also record before/after targeted Test262 results. If
`ES_CONFORMANCE.md` is absent or stale, generate it with `just test262-conformance`
after a representative run rather than relying on memory.

### 3. Preserve active-stack boundaries

When porting behavior out of parked compatibility crates, move semantics first
and adapter glue second:

- Active VM/runtime code stays in `crates/otter-gc`, `crates/otter-vm`,
  `crates/otter-runtime`, and `crates/otter-jit`.
- Standards-facing Web APIs belong in `crates/otter-web`.
- Otter-hosted modules belong in `crates/otter-modules`.
- Parked compatibility crates must not become a second runtime stack.

### 4. Prefer representation tables for recurring choices

Repeated translation choices should be explicit. Keep local tables for high-risk engine choices:

- JS value and object representations: `RegisterValue`, GC references, object
  shapes, property key ordering, prototype lookup caches.
- String/data boundaries: JS strings, UTF-16/UTF-8 conversion, raw bytes from
  host APIs, path/env/network data.
- Collections: ordered maps for spec-visible iteration; hash maps only where
  order is irrelevant.
- Async boundaries: VM job queue, Tokio worker work, promise settlement, and
  callbacks back onto the runtime thread.
- FFI/host objects: ownership, thread-affinity, GC rooting, and Send/Sync rules.

### 5. Add a small status block to large ports

For substantial ports from parked crates, compatibility shims, or reference
implementations, end the new or modified module with a short status block in a
comment when it helps review:

```rust
// PORT STATUS
//   source:     <parked crate/file or reference area>
//   confidence: high | medium | low
//   todos:      N
//   tests:      <targeted tests or reason omitted>
//   notes:      <one line for reviewers>
```

Use this only for real ports or large semantic migrations. Do not add it to
small bug fixes.

## Otter-Specific Adaptations

### Async

Otter uses Tokio at the runtime integration boundary. The rule is:

- Worker tasks may perform Rust async work.
- VM/JS interaction must hop back to the runtime scheduling boundary.
- Promise settlement must go through the target VM/runtime job queue.
- Timers are runtime primitives, not Node-specific backends.

### Unsafe and GC

Otter already has strong unsafe hygiene in active crates. Keep extending it:

- Every unsafe block has an adjacent `// SAFETY:` comment.
- Every public `unsafe fn` has a `# Safety` section.
- Any value stored across allocations, host calls, async hops, or GC safepoints
  must be rooted or otherwise proven live.
- Do not assert `Send`/`Sync` for host objects that carry VM pointers, JS values,
  runtime state, FFI callback state, or thread-affine handles unless the proof is
  documented next to the impl.

### Strings and bytes

For Web/Node compatibility work, treat external data carefully:

- Do not validate path, env, network, or FFI bytes as UTF-8 unless the API
  contract explicitly requires it.
- Keep JS string encoding decisions at the VM/runtime boundary.
- Avoid `String::from_utf8(...).unwrap()` and lossy conversions in runtime-visible
  behavior.

### Collections

Spec-visible order is a correctness property:

- Use ordered collections or explicit sorting for JSON output, property order,
  module namespace exports, URL/search params, headers, and iterator-visible
  results.
- Use unordered hash maps only when tests prove order cannot leak.

## Suggested Local Audit Commands

These are reporting commands, not hard gates yet. Turn them into CI gates only
after reviewing current false positives.

```bash
# Porting debt markers.
grep -R "TODO(port)\|PERF(port)\|PORT NOTE\|PORT STATUS" crates docs --line-number

# Reachable placeholders that should not ship in runtime code.
grep -R "todo!()\|unimplemented!()" crates --include '*.rs' --line-number

# Unsafe blocks without nearby safety text need manual review.
grep -R "unsafe" crates/otter-gc crates/otter-jit crates/otter-modules --include '*.rs' --line-number

# Runtime-visible lossy or panic-prone UTF-8 conversions.
grep -R "from_utf8.*unwrap\|from_utf8_lossy\|to_string()" crates/otter-vm crates/otter-runtime crates/otter-web crates/otter-modules --include '*.rs' --line-number

# Potentially nondeterministic maps in spec-visible crates.
grep -R "HashMap\|FxHashMap" crates/otter-vm crates/otter-runtime crates/otter-web crates/otter-modules --include '*.rs' --line-number
```

## Recommended Next Changes

1. Add a `just process-audit` recipe that runs the reporting commands above and
   exits successfully while the baseline is noisy.
2. Add a CI-only stricter follow-up once the baseline is triaged:
   `just process-audit-strict`.
3. Restore or regenerate `ES_CONFORMANCE.md` if it is intentionally referenced
   by `AGENTS.md`; otherwise update `AGENTS.md` to point at the current
   conformance artifact.
4. For the next large Web API or Node API port, require a small before/after
   note: conformance baseline, marker count, touched type declarations, and
   targeted tests.
