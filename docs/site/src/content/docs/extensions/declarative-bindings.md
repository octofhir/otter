---
title: "Declarative Bindings: Classes, Namespaces, Extensions"
---

This is the standard way to add a JS-visible surface to Otter — a host
class, a namespace, or a whole extension bundle. Declare the surface
once; the framework owns prototype linkage, `instanceof`, branding,
argument extraction, return-value construction, async, and install
choreography. If you are writing `value_roots`, hand-rolled receiver
checks, or a global-name registry — stop and use this layer.

The model to emulate is [handle scopes](/extensions/handle-scopes/):
the safe path is the default path. Here the declaration *is* the
descriptor — parameter types declare extraction, return types declare
construction, `async` declares the promise protocol, and JS names are
always explicit.

## A host class in one declaration

```rust
use std::sync::Arc;
use otter_macros::{FromJs, HostClass, js_class};
use otter_runtime::marshal::{ArrayBuffer, BufferSource, Sequence, USVString, Uint8Array};

/// Plain Rust data — this struct IS the JS instance's backing state.
#[derive(Debug, Clone, HostClass)]
pub struct Blob {
    bytes: Arc<[u8]>,
    content_type: String,
}

#[js_class(name = "Blob", feature = WEB)]
impl Blob {
    #[constructor]
    fn js_new(parts: Option<Sequence<BlobPart>>, options: Option<BlobPropertyBag>) -> Blob {
        // Body sees only Rust data. Return the instance DATA; the
        // engine allocates the object with the right prototype
        // (new.target-aware, so JS subclasses just work).
        Blob::from_parts(parts, options)
    }

    #[getter(name = "size")]
    fn js_size(&self) -> f64 { self.bytes.len() as f64 }

    #[method(name = "slice", length = 2)]
    fn js_slice(&self, start: Option<f64>, end: Option<f64>) -> Blob {
        // Returning a declared class mints a real branded instance.
        /* … */
    }

    #[method(name = "bytes")]
    async fn js_bytes(self) -> Uint8Array {          // → Promise<Uint8Array>
        Uint8Array(self.bytes.to_vec())
    }
}
```

Free by construction: correct prototype + `instanceof` (including
`class Mine extends Blob` in user JS — `super()` honors `new.target`),
`Symbol.toStringTag`, brand-checked receivers with spec error
messages, WebIDL argument coercion, promise-returning async methods.

### Member markers

| Marker | Meaning |
|---|---|
| `#[constructor]` | Exactly one. No receiver; returns `Self` or `Result<Self, JsError>`. |
| `#[method(name = "…")]` | Options: `length = N` (default: count of leading non-`Option` params), `promise` (wrap a sync result in a fulfilled promise), `raw` (see below). |
| `#[getter(name = "…")]` / `#[setter(name = "…")]` | Prototype accessor halves; same-name halves merge. Getters take no params and return owned data; setters take exactly one param. |
| `#[static_method(name = "…")]` | Own data property on the constructor (`URL.parse`, `URL.canParse`). No receiver; same `length`/`promise`/`raw`/`async` options as methods. |

Receivers: `&self` for reads, `&mut self` for mutation (URL setters),
owned `self` only on `async fn` (the glue clones a snapshot — nothing
GC-touching may cross an `.await`; requires `Clone`).

Fallible bodies return literally `Result<T, JsError>` (`JsError::Type`
/ `Range` / `Dom { name, message }`); the operation name is prefixed
automatically.

### Native subclassing

```rust
#[derive(Debug, Clone, HostClass)]
pub struct File {
    #[host_class(parent)]   // ancestry: Blob methods run on File instances
    blob: Blob,
    name: String,
}

#[js_class(name = "File", feature = WEB, extends = Blob)]
impl File { /* constructor + own members */ }
```

List the parent class before the subclass at registration; the
subclass resolves `Blob.prototype` / the `Blob` constructor off the
global at install.

### The escape hatch: `raw`

```rust
#[method(name = "fastPath", length = 1, raw)]
fn fast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { /* … */ }
```

`raw` members are the fn itself — no glue, no conversions — on the
identical install path. Hot paths keep hand-tuned bodies; everything
else never needs this.

## Argument shapes: `FromJs` derives

```rust
#[derive(Debug, Default, FromJs)]           // WebIDL dictionary
pub struct BlobPropertyBag {
    #[js(name = "type", default)]           // field name used verbatim otherwise
    content_type: USVString,
    #[js(name = "lastModified")]
    last_modified: Option<f64>,             // Option = optional member
}

#[derive(FromJs)]                            // WebIDL union
pub enum BlobPart {
    Blob(Blob),            // brand-probed (declared classes probe automatically)
    Buffer(BufferSource),  // ArrayBuffer | typed-array view, bytes copied
    Text(USVString),       // catch-all coercion — must be LAST
}
```

Dictionary members read in lexicographic order (the observable WebIDL
order); a non-`Option` field without `#[js(default)]` is required.
Union variants probe by *type test* in declaration order — never by
trial coercion.

Built-in extraction types: scalars with spec coercions, `String` /
`USVString` (USV), `DOMString` (WTF-16 preserving), `Sequence<T>` (any
iterable), `BufferSource`, `HostRef<'s, T>` (zero-copy brand-checked
view), `Callback<'s>`, `JsValue<'s>` (raw handle escape). Construction
(`IntoJs`): scalars, strings, `Option` (nullable), `Vec<T>`,
`ArrayBuffer(Vec<u8>)`, `Uint8Array(Vec<u8>)`, declared classes,
`#[derive(IntoJs)]` structs.

## Async methods

`async fn` compiles to the full promise protocol: sync prologue on the
isolate (snapshot + extraction), a `Send` future on the shared Tokio
runtime, and a completion job that converts the result and settles the
promise back on the isolate. An immediately-ready future (a data-only
method) settles with **no executor round-trip**. Rejections surface as
real error instances.

```rust
#[method(name = "wait")]
async fn js_wait(self, ms: f64) -> Result<String, JsError> {
    tokio::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
    Ok(format!("{}+{}", self.label, ms))
}
```

## Namespaces

```rust
pub struct WebCrypto;   // marker type

#[js_namespace(name = "crypto", feature = WEB, tag = "Crypto", js = "crypto.ns.js")]
impl WebCrypto {
    #[method(name = "randomUUID")]
    fn random_uuid() -> Result<String, JsError> { /* … */ }

    #[method(name = "__nativeDigest")]
    async fn native_digest(algorithm: String, data: BufferSource)
        -> Result<ArrayBuffer, JsError> { /* … */ }
}
```

Members are static (no receiver); `raw`, `promise`, and `async` work
exactly as on classes. `tag = "…"` pins `@@toStringTag` on the
namespace object.

## Hosted modules

The module counterpart — same shape, `lodge!` machinery underneath:

```rust
pub struct MathModule;   // marker type

#[js_module(prefix = "test", name = "math", capabilities = true)]
impl MathModule {
    #[export(name = "add")]
    fn add(a: f64, b: f64) -> f64 { a + b }

    #[export(name = "slowDouble")]
    async fn slow_double(n: f64) -> f64 { /* Tokio await … */ }

    #[export(name = "openStore")]
    fn open_store(caps: &CapabilitySet, path: Option<USVString>)
        -> Result<KvStore, JsError> { /* … */ }
}

// registration: builder.hosted_module(MATH_HOSTED_MODULE)
// JS: import { add, slowDouble } from "test:math";
```

`prefix`/`specifier` and `name` are explicit. With `capabilities =
true`, an export may declare `caps: &CapabilitySet` as its first
parameter to receive the install-time snapshot; argument-derived
checks (path allowlists against a real argument) stay in the body —
the framework provides the snapshot, never guesses the check.

## Attached JS: the class's JS half

Some members are genuinely better in JS (composition over other Web
classes, exact `DOMException` identities). Attach the file next to the
declaration:

```rust
#[js_class(name = "URL", feature = WEB, js = "url.class.js")]
```

The source evaluates immediately after the native install — the two
halves are one unit, so a JS-defined member can never be missing next
to a live native class. Pattern: consume private `__native*` members,
wrap them with spec validation, `delete` them (see
`crates/otter-web/src/crypto.ns.js`).

## Bundling: `romp!` extensions

```rust
otter_macros::romp! {
    name = "web",
    ident = WEB_EXTENSION,
    classes = [url::WebUrlIntrinsic, blob::BlobIntrinsic, blob::FileIntrinsic],
    js = [
        (include_str!("web_bootstrap.js"), defines = ["Event", "EventTarget", /* … */]),
        (include_str!("web_fetch.js"),     defines = ["Headers", "Request", "Response"]),
    ],
}

// registration — one line:
builder.extension(&WEB_EXTENSION)
```

Classes install eagerly in declaration order. Every `js` source
registers under **native lazy globals**: per-name accessors on
`globalThis` whose first read evaluates the sources once and reads the
real global back. `defines` is the declaration of what each source
installs — keep it honest with a def-scan test (see
`lazy_global_names_match_shim_def_calls` in `otter-web`). There is no
other registry to maintain.

## Testing your surface

Same contract as handle scopes — every declared surface must hold
under GC stress:

```bash
cargo build -p otter-cli
for s in 0 1 2 4 8 16; do
  OTTER_GC_STRESS=$s target/debug/otter run exercise.mjs
done
# CHECK EXIT CODES AND LINE COUNTS, not just sorted output — a silent
# death produces no line and hides in `sort -u`.
```

And drive the JS-visible behavior end to end: `instanceof`, a JS
subclass, `Object.prototype.toString.call(x)`, promise methods through
`.then`, and (for extensions) a cold start that touches exactly one
lazy name.

## When to stay manual

Per the project rules: capability *sequencing* beyond a boolean gate,
non-obvious install order, and profiled hot paths stay hand-written
(`raw` members, `burrow!`, hand installers). The declarative layer is
the default, not a mandate.

## See also

- [Handle Scopes](/extensions/handle-scopes/) — the rooting contract
  the generated glue rides.
- `EXTENSION_API_PLAN.md` (repo root) — the full design, VM additions,
  and migration state.
- `crates/otter-web/src/blob.rs`, `url.rs`, `crypto.rs` — the worked
  exemplars.
