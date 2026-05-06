# Otter Engine Contributor Guide

This book is the contributor-facing guide for Otter's active new engine.
It explains how to extend the runtime without copying internals from task
files, parked crates, or raw GC adapters.

Use these sources together:

- this book: stable contributor workflows and examples;
- [`docs/new-engine/tasks/`](../../new-engine/tasks/README.md):
  implementation slices, current migration state, and closeout notes;
- [`docs/new-engine/adr/`](../../new-engine/adr/): accepted architecture
  decisions;
- [`AGENTS.md`](../../../AGENTS.md): repository rules for coding agents.

Task files are not the long-term API manual. When a contributor-facing API
stabilizes, move the workflow here and leave the task file as historical
context.

## Local Build

Build the book with:

```bash
mdbook build docs/book
```

The docs examples that exercise current GC APIs are backed by Rust tests in
`crates-next/otter-gc`; run them with the normal task gates:

```bash
cargo test -p otter-gc
```

If `mdbook` is not installed, install it with normal Rust tooling outside
this repository. CI wiring is tracked in
[`Task 95`](../../new-engine/tasks/95-contributor-book-and-extension-guides.md).
