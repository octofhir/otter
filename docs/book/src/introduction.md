# Otter Engine Contributor Guide

This book is the contributor-facing guide for Otter's new engine.

It complements the task files and ADRs:

- task files describe implementation slices and migration state;
- ADRs record architectural decisions;
- this book describes stable workflows for contributors, extension
  authors, macro authors, and future plugin authors.

The initial book is intentionally small. Pages should grow when APIs
stabilize, especially the GC/session API, hosted module API, and macro
surface.

## Build

```bash
mdbook build docs/book
```

If `mdbook` is not installed, install it with your normal Rust tooling
outside this repository. CI wiring is tracked in
`docs/new-engine/tasks/95-contributor-book-and-extension-guides.md`.
