---
title: "Embedding: Writing an Extension"
---

This guide is for embedders: you ship an application that hosts Otter
and you want your own JS-visible surface — a product namespace like
`Acme`, custom host classes, your own `myapp:*` modules. You write
plain Rust bodies; the declarative layer owns GC rooting, prototype
linkage, argument marshalling, async plumbing, and install
choreography. The declaration forms themselves are documented in
[Declarative Bindings](/extensions/declarative-bindings/); this page
covers the embedder-specific wiring.

**Start from the template**: `examples/extension-template/` in the
Otter repository is a complete, tested extension crate — one class,
one namespace, one hosted module, one `romp!` bundle, plus the tests
that prove them. Copy it and rename.

## Crate setup

```toml
[dependencies]
otter-gc = { version = "…" }
otter-macros = { version = "…" }
otter-runtime = { version = "…" }
```

One line of linking convention at your crate root:

```rust
// Macro-generated glue resolves `::otter_vm::…` paths through this
// alias in binding crates.
extern crate otter_runtime as otter_vm;
```

Everything you need is re-exported from `otter_runtime`: the
`marshal` module (`JsError`, `USVString`, `BufferSource`,
`Uint8Array`, …), `CapabilitySet`, `Extension`, `GlobalClass`,
`HostedModule`, and the builders.

## Declare, bundle, register

Declare surfaces with `#[js_class]` / `#[js_namespace]` /
`#[js_module]` exactly as in [Declarative
Bindings](/extensions/declarative-bindings/) — `feature = WEB` is the
right bit for embedder surfaces. Bundle the globals with `romp!`:

```rust
romp! {
    name = "acme",
    ident = ACME_EXTENSION,
    classes = [CounterIntrinsic, AcmeIntrinsic],   // classes AND namespaces
    js = [],                                       // optional pure-JS members
}
```

Register on your builder — extensions carry the globals, hosted
modules register separately:

```rust
// Direct mode (no event loop — sync surfaces only):
let mut runtime = RuntimeBuilder::default()
    .extension(&ACME_EXTENSION)
    .build()?;

// Full embedding (event loop, async methods, module imports):
let otter = Otter::builder()
    .extension(&ACME_EXTENSION)
    .hosted_module(UTIL_HOSTED_MODULE)
    .build()?;
otter.handle().run_module("main.mjs").await?;
```

Adding another namespace later is one `#[js_namespace]` impl plus one
`romp!` row. Adding a class is one struct + one impl + one row. That
is the whole cost model.

## High-level global installers and realms

Public embedding code should not touch `Interpreter`, `Value`, `JsObject`,
GC handles, or write barriers. A `RuntimeGlobalInstaller` receives a
`RuntimeRealmContext`, whose deliberately small surface installs owned primitive
configuration, registers extension natives, runs trusted bootstrap source,
snapshots capabilities, and obtains owned task delivery:

```rust
fn install_acme(realm: &mut RuntimeRealmContext<'_>) -> Result<(), OtterError> {
    realm.install_global("acmeVersion", env!("CARGO_PKG_VERSION"))?;
    realm.install_script(SourceInput::from_javascript(
        "Object.defineProperty(globalThis, 'acmeVersion', { writable: false });",
    ))
}

let builder = Runtime::builder()
    .global_installer(RuntimeGlobalInstaller::new(install_acme));
```

The same installer and configured `Extension` classes/JS run for the default
realm and every additional realm. Realm identity is the opaque, owned,
`Send + Sync` `RuntimeRealmId`; it contains no GC pointer:

```rust
let realm = otter.create_realm().await?;
let result = otter.run_script_in_realm(
    realm,
    SourceInput::from_javascript("globalThis.frameState = 1"),
    "frame:initial",
).await?;
```

Globals are isolated and repeated turns retain state in their target realm.
Classic-script execution is realm-aware today. Realm-targeted module graphs and
explicit realm disposal are later high-level additions; embedders must not work
around them by retaining raw globals.

## What works where

| Surface | Direct mode (`RuntimeBuilder::build`) | Handle mode (`Otter::builder`) |
|---|---|---|
| Classes, namespaces, sync methods | ✓ | ✓ |
| `async fn` members | Only immediately-ready bodies (the poll-once fast path) | ✓ full protocol (Tokio + event loop) |
| Hosted-module imports | — (needs the module runner) | ✓ |
| Timers, servers | — | ✓ |

Async methods in direct mode reject with a `TypeError` if the future
actually suspends — there is no executor to drive it. Data-only async
bodies (no real `.await`) work everywhere.

## Capabilities

Deny-by-default applies to your extension with no extra wiring. With
`capabilities = true` on a `#[js_module]`, an export takes the
install-time snapshot as its first parameter:

```rust
#[export(name = "canReadEnv")]
fn can_read_env(caps: &CapabilitySet, name: USVString) -> bool {
    caps.env_allows(name.as_str())
}
```

Boolean gates read the snapshot; argument-derived checks (a path
allowlist against a real argument) belong in the body too — the
framework provides the snapshot, never guesses the check.
Embedder-defined capability *kinds* are out of scope: built-in kinds
only; custom policy is body-level logic against your own state.

## GC-stress verification

Every new surface must hold up under a moving collector. The
generated glue is sound by construction, but your `raw` members and
attached JS are yours to prove:

```bash
for s in 0 1 2 4 8 16; do
  OTTER_GC_STRESS=$s otter run exercise.mjs
done
```

Compare **exit codes and line counts** across strides — a silent
death produces no output line and hides in `sort -u`-style diffing.
Exercise `instanceof`, a JS subclass of your class,
`Object.prototype.toString.call(x)`, promise methods through `.then`,
and a module import.

## Stability

The authoring kit is source-level stable under semver. There is no
ABI/dylib plugin story: extensions compile into your embedding.

## See also

- [Declarative Bindings](/extensions/declarative-bindings/) — the
  declaration forms in depth.
- [Handle Scopes](/extensions/handle-scopes/) — the rooting contract
  under `raw` members.
- `examples/extension-template/` — the runnable starting point.
