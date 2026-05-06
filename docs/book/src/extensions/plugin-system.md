# Plugin System

The plugin system is future work. This page records direction and
boundaries so new extension APIs do not block it. It is not a stable ABI
promise.

Plugins should eventually be able to add hosted modules, native bindings,
and host-owned object surfaces without depending on GC internals.

Design constraints:

- plugin APIs should be safe by default;
- raw collector internals are not part of the normal plugin API;
- long-lived JS references use persistent roots;
- plugin-owned buffers use external-memory accounting;
- async plugin work must re-enter the owning isolate before touching JS
  values;
- dynamically loaded plugins, if supported, need an explicit ABI and
  versioning story.

## Layering Direction

The supported layers should arrive in this order:

1. in-workspace hosted modules using current native/context APIs;
2. native bindings compiled with the engine and installed through JS
   surface builders;
3. out-of-tree Rust plugin packages that depend on a stable extension
   crate;
4. optional dynamic ABI/FFI plugins, only after versioning, safety, and
   ownership rules are explicit.

The first two layers are source-level Rust APIs. They may change while the
engine is under `crates-next/*`. Dynamic plugins require a much stricter
compatibility contract and are deferred.

## Non-Negotiables

Plugin-facing APIs must not expose raw collector internals by default.
Persistent JS-visible state uses roots, weak handles upgrade through a
matching branded context, external memory is accounted through RAII, and
async work re-enters the owning isolate before touching JS values.

Plugins may request capabilities, but the runtime decides whether those
capabilities are granted. Permission checks must remain explicit and
testable at the Rust boundary.

Plugin details stay design-only until the plugin API becomes concrete.
