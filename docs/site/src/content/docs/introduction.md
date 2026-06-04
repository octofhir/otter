---
title: "Otter Engine Contributor Guide"
---

This book is the contributor-facing guide for Otter's active new engine.
It explains how to extend the runtime without copying internals from task
files, parked crates, or raw GC adapters.

Use this book for stable contributor workflows and examples. Repository rules
for coding agents live in [`AGENTS.md`](https://github.com/octofhir/otter/blob/main/AGENTS.md).

Historical task and ADR files are intentionally excluded from the living docs.
When a contributor-facing API stabilizes, its workflow belongs here.

## Local Build

Build the book with:

```bash
mdbook build docs/book
```

The docs examples that exercise current GC APIs are backed by Rust tests in
`crates/otter-gc`; run them with the normal task gates:

```bash
cargo test -p otter-gc
```

If `mdbook` is not installed, install it with normal Rust tooling outside
this repository.
