# Task 58 — runtime-lazy `import()` for non-literal specifiers

## Goal

Lift the foundation restriction that `import(expr)` only
accepts string-literal specifiers. After this slice, dynamic
`import()` accepts any expression evaluating to a string at
runtime, parses + compiles + links the target module on
demand, and settles the returned promise spec-correctly.

## Context

Tasks 36a / 36b established **eager** module loading: every
static import and every literal-string `import("./x")` is
resolved at compile time, the full graph is linked into a
single `BytecodeModule`, and `<entry>` evaluates module inits
in post-order DFS. Non-literal `import(variable)` is rejected
at compile time with `MODULE_DYNAMIC_NON_LITERAL`.

That decision was made in 36a to avoid threading lazy compile
through the dispatcher while we built up the rest of the
foundation. Once the engine is otherwise feature-complete we
need the lazy path back: it's spec-mandated, real codebases
depend on it (route-level code splitting, conditional polyfill
loaders, plugin loaders).

## Scope

- Remove the `MODULE_DYNAMIC_NON_LITERAL` compile-time
  rejection; the compiler emits `Op::ImportCall dst, src_reg`
  for any `import(expr)`.
- Runtime helper invoked from `Op::ImportCall`:
  1. Reads specifier from `src_reg` at runtime.
  2. Resolves through the loader using the caller's module
     URL as referrer.
  3. If the URL is already registered (eager-loaded or
     previously lazy-loaded): fulfil the returned promise with
     the cached `module_env`.
  4. Otherwise: enter parse → compile → link → init pipeline
     for the new module, splice it into the existing
     interpreter state (registry + linker side-tables), run
     its `<module-init>`, fulfil the promise with the freshly-
     populated `module_env`.
- The lazy path must work cooperatively with the dispatcher
  — same `&mut Interpreter`, same microtask queue. Think
  through: what happens when a lazy-loaded module's
  `<module-init>` itself does another lazy `import()`? The
  outer promise resolves only after the inner one does;
  microtask ordering must remain spec-correct.
- Live bindings between an eager-loaded module and a
  lazy-loaded one still work: same `module_env` JsObject
  shape, same property loads at every reference site.
- Cycle handling for lazy-loaded modules: detect re-entry
  during the lazy `<module-init>` and reject the promise
  with the same `RangeError`-shaped diagnostic eager loading
  uses.
- Capability check: lazy `import()` consults the same `fs_read`
  capability as eager loading. Denial rejects the promise.

## Out of scope

- HTTP-based imports (still deferred — separate loader
  concern; see 36b).
- Top-level `await` (still deferred).
- Worker-thread module loading.

## Files / directories you may touch

- `crates-next/otter-runtime/` — extend the linker / module
  registry to support post-`<entry>` module insertion.
- `crates-next/otter-vm/` — `Op::ImportCall` runtime path.
- `crates-next/otter-compiler/` — drop the literal-only check.
- `tests/engine/modules-lazy/` — fixtures: lazy import of a
  not-statically-referenced module, conditional load,
  recursive lazy load, lazy-import cycle detection.

## Acceptance criteria

- `let name = "./b.ts"; let m = await import(name);` works
  when wrapped in an async fn (top-level await still
  out-of-scope; the test uses an `async function` driver).
- A module not statically reachable from the entry can be
  loaded by name at runtime.
- A lazy-import cycle rejects the returned promise with
  `RangeError`.
- Capability denial rejects the promise rather than throwing
  synchronously from `Op::ImportCall`.
- Engine suite green.

## Risks

- Splicing a freshly-compiled module's function table into a
  running `BytecodeModule` requires either (a) growing the
  function vector (read by `dispatch_loop` via `&BytecodeModule`,
  so growing in place might invalidate references) or
  (b) replacing the module reference between dispatch
  iterations. Pick (b): the linker maintains an `Rc<RwLock<…>>`
  or similar around the module so dispatch always sees the
  current view at the top of the loop.
- Memory: lazy-loading the same module from many sites must
  parse + compile only once. The registry already handles
  "already loaded" — verify the parse / compile entry points
  don't bypass it.

## Status

- blocked on tasks 36a + 36b
