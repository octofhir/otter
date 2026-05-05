# GC API

Otter's GC API should balance Rust safety with a public model that is
easy to extend.

The target contributor-facing model is:

- `Local`: stack-scoped rooted handle;
- `Root`: persistent isolate-owned root;
- `Weak`: weak handle upgraded only through the matching GC session;
- `GcSession` / `RuntimeCx` / `NativeCtx`: explicit context for
  allocation, mutation, rooting, and weak upgrade;
- safe mutation wrappers that perform write barriers automatically;
- RAII external-memory accounting for backing stores.

Normal engine and extension code should not use raw GC pointers,
manual barriers, raw slot visitors, or handle-table internals. Those
belong in `otter-gc` internals and audited VM adapter layers.

The detailed design is tracked in:

- `docs/new-engine/tasks/93-gc-branded-session-api.md`;
- `docs/new-engine/tasks/94-gc-contributor-api-surface.md`.
