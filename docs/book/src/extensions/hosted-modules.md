# Hosted Modules

Hosted modules expose native Rust functionality to JavaScript through
the runtime.

Use hosted modules for Otter-owned APIs such as:

- `otter:kv`;
- `otter:sql`;
- `otter:ffi`;
- future standard-facing or runtime-specific modules.

Hosted modules must enforce capabilities at the Rust boundary. Do not
trust JavaScript wrappers or TypeScript declarations as the only
permission check.

Use the active macro and runtime APIs when possible. If capability
enforcement or bootstrap order is delicate, prefer explicit manual code
over hiding control flow behind a macro.
