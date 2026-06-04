---
title: "Web API Contribution Workflow"
---

Web APIs live in `crates/otter-web` on the active runtime stack.

Current active slices:

- `URL`: parsing, relative resolution, and common URL parts.
- `Headers`: normalized ordered header list.
- `Blob`: owned byte payloads, `size`, `type`, `slice`, and text decoding.
- `Request` / `Response`: owned Fetch-shaped records.

Expose Web API constructors, prototypes, and globals through static
`ClassSpec` / builder data. Keep installation centralized; do not mutate
`globalThis` from unrelated modules and do not add a separate Web runtime
stack.

Rules for new Web API work:

- store host state as owned Rust data, not VM contexts or handles;
- validate arguments on the isolate thread;
- enforce `net`, `read`, or other capabilities at the Rust boundary before
  starting host work;
- copy owned request/body data into async futures, then post completions back
  to the isolate;
- add focused Rust tests for host-side records and JS/module tests for
  JS-visible behavior.

Macros are appropriate when they generate the same static specs and builder
calls a manual implementation would write. Keep manual code when capability
checks, bootstrap order, async scheduling, or host-owned object lifetimes are
the main behavior.
