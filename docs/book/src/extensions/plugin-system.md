# Plugin System

The plugin system is future work. This page records the direction so
new extension APIs do not block it.

Plugins should eventually be able to add hosted modules, native
bindings, and host-owned object surfaces without depending on GC
internals.

Design constraints:

- plugin APIs should be safe by default;
- raw collector internals are not part of the normal plugin API;
- long-lived JS references use persistent roots;
- plugin-owned buffers use external-memory accounting;
- async plugin work must re-enter the owning isolate before touching JS
  values;
- dynamically loaded plugins, if supported, need an explicit ABI and
  versioning story.

Task tracking lives in
`docs/new-engine/tasks/95-contributor-book-and-extension-guides.md`
until the plugin API becomes concrete.
