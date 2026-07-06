---
title: "Dependency Policy"
---

Otter should have a thin engine core and dependency-rich edges. Dependencies
are not bad by themselves, but their crate layer matters: a crate that is fine
in the CLI or Web API layer can make the VM harder to embed, slower to build,
or harder to audit if it leaks downward.

## Layer Budget

`otter-gc` is the narrowest layer. Keep it close to `std` plus small
data-structure or platform crates needed for collector mechanics. Platform
calls such as `libc::madvise` are acceptable when they are isolated, documented,
and do not become contributor-facing APIs.

`otter-bytecode` should stay format-focused: bytecode data structures,
deterministic serialization helpers, and disassembly. It must not depend on
runtime, host, async, networking, package-manager, or CLI crates.

`otter-vm` may use parser-independent data-structure crates and the active
engine support crates, but it must not pull in `tokio`, `reqwest`, CLI
diagnostics, package-manager logic, Node/Web API crates, or compatibility
shims. VM behavior should stay embeddable without a host runtime.

`otter-runtime` owns scheduling, permissions, workers, host state, and the
embedding surface. Async runtime dependencies belong here, not in the VM or GC.

Product/API crates such as `otter-cli`, `otter-web`, `otter-node`,
`otter-modules`, and `otter-pm` may carry heavier dependencies when the API
requires them. Those dependencies must not flow back into the active engine
core.

## Dependency Direction

Keep the active direction:

```text
otter-gc -> otter-vm -> otter-runtime -> product/API crates
```

Do not add path dependencies from active crates into `crates-legacy/*`, and do
not introduce a parallel engine or runtime stack to make a dependency easier to
reach.

## Native Code Versus JS Glue

Rust-native built-ins are not automatically faster. A native function crosses
the JS/Rust boundary, coerces `Value`s, handles re-entrancy, roots GC values,
records write barriers, and often allocates anyway. For small orchestration
logic, a JS shim over a few narrow native primitives can be faster to iterate
on and closer to the spec's observable order.

Use Rust-native functions when at least one of these is true:

- the operation enforces a capability or host boundary;
- the operation needs direct access to external resources, buffers, clocks,
  entropy, files, sockets, or platform APIs;
- the operation is a low-level primitive that JS cannot express efficiently;
- a benchmark or profile shows JS glue is a real bottleneck;
- the operation needs privileged VM/GC access that cannot be exposed safely to
  JS.

Prefer JS glue when:

- the behavior is mostly spec orchestration over existing primitives;
- the important part is order of observable property reads, calls, or
  exceptions;
- the API can be expressed as a wrapper around a small native primitive;
- the logic is large, rarely hot, or expected to change with standards work.

When in doubt, start with the smaller native primitive and put public behavior
in JS. Move logic native only after measurement or a clear boundary reason.

## Adding A Dependency

Before adding a dependency, answer:

- Which layer owns it?
- Can the dependency be isolated in a product/API crate instead of the VM or GC?
- Does it affect startup, binary size, platform support, or audit surface?
- Does it duplicate functionality already available in the active stack?
- Is deterministic behavior required, and does the crate preserve it?

For core crates, include the reason in the PR or task closeout. For dependencies
that cross a lower boundary, add a short note to this document or the owning
engine page explaining why the lower layer is the right place.

## Review Checklist

- `cargo tree -p <crate> -e normal` for the touched crate.
- No new downward dependency into product/API, CLI, PM, or legacy crates.
- No async/network/process dependency in GC, bytecode, or VM.
- Public APIs expose owned DTOs or safe handles, not runtime-internal mutable
  state.
- New native APIs have a boundary/performance reason, not only convenience.
