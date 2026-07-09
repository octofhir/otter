# Extension API: declarative JS-visible surfaces

Design for a high-level, safe-by-default authoring layer for every JS-visible
surface in Otter — host classes, namespaces, global functions, hosted modules,
and the values they exchange. The model to emulate is the handle-scope layer:
`ctx.scope` + `Scoped<'s>` made the safe rooting path the default path and let
lifetimes enforce it. This layer does the same for *declaring* surfaces and
*marshalling* values: you declare the JS shape once, in one place, and the
framework owns prototype linkage, branding, argument extraction, return-value
construction, async, capability gates, and lazy install.

Breaking changes are allowed. `couch!` / `holt!` (as declaration forms) are
replaced; the underlying spec/installer machinery they compile to is reused.
`raft!`, `burrow!`, `#[derive(Pelt)]`, `#[derive(Groom)]` are unaffected.
`lodge!` and `#[dive]` are extended, not replaced.

The layer serves **three tiers with one declaration form**: ECMAScript spec
builtins (`Map`, `Number`, `Proxy`, … — `feature = CORE`, §10), platform APIs
(Web/WinterTC — the main body of this document), and **embedder-defined
surfaces** (custom top-level namespaces like an `Otter`/`Acme` global, custom
classes, custom `myapp:*` hosted modules — §11). Adding a new namespace or
class must be one file plus one registration row at every tier.

Status: design only. Nothing here is implemented.

---

## 1. The seven pains, and where each dies

| # | Pain today | Where it dies |
|---|---|---|
| 1 | Manual prototype linkage (`link_class_prototype`, null-proto instances, `Object.setPrototypeOf` in JS subclass shims) | VM-owned `[[Construct]]` for host classes (§3.2): the engine allocates the instance with `new.target.prototype` *before* the native body runs |
| 2 | No uniform "return a JS value" (`string_value` vs `array_buffer_from_bytes_rooted` vs `fulfilled_promise_with_roots`; no Uint8Array builder at all) | `IntoJs` trait + new scoped builders (§2, §6) |
| 3 | Argument extraction dance (BufferSource, options dicts, sequences by hand) | `FromJs` trait + derives + `MarshalCx` (§2) |
| 4 | Two parallel worlds: native classes vs lazy JS shims, with shim-load ordering hazards | `romp!` extension bundles: natives + JS glue are one lazily-materialized unit (§5) |
| 5 | Hand-maintained `WEB_GLOBAL_NAMES`, eval-built accessor shims, one-off installers | Extension descriptor derives the name list; native lazy-accessor installer (§5) |
| 6 | Async is a manual promise wrap; `#[dive(deep)]`'s relationship to classes unclear | `async fn` methods with a typed completion protocol (§3.5) |
| 7 | GC boilerplate leaks into every builder (`*_object` snapshot dances) | Generated glue runs entirely inside one handle scope; user code touches only Rust data and `Scoped` handles (§2.4) |

---

## 2. Marshalling layer: `FromJs` / `IntoJs` / `MarshalCx`

Lives in `otter-vm` (core traits + primitive impls) and is re-exported through
`otter-runtime` for binding crates. `otter-web` sees only these types — no raw
`Value` juggling, no `Rc`/`RefCell`, no VM internals.

### 2.1 `MarshalCx`

The single context handed to conversions and generated glue. It bundles the
things every conversion needs and nothing else:

```rust
/// Borrowed view over (&mut NativeCtx<'rt>, &'s HandleScope).
/// Never crosses `.await`; `'s` pins every handle it mints.
pub struct MarshalCx<'rt, 'cx, 's> {
    ctx: &'cx mut NativeCtx<'rt>,
    scope: &'s HandleScope,
}
```

It exposes the existing `scoped_*` surface plus the new primitives from §6
(spec coercions, typed-array builders, promise builders, brand-checked host
data). User-visible only when writing a *manual* impl of `FromJs`/`IntoJs`
or a `raw` method — the derive/macro path never shows it.

### 2.2 `FromJs` — argument extraction

```rust
pub trait FromJs<'s>: Sized {
    /// WebIDL-style conversion. `ident` names the value in errors
    /// ("Blob constructor, argument 1", "member 'type'").
    fn from_js(cx: &mut MarshalCx<'_, '_, 's>, v: Scoped<'s>, ident: ValueIdent<'_>)
        -> Result<Self, JsError>;
}
```

Provided impls (WebIDL coercion semantics, re-entrant `valueOf`/`toString`
handled through the new scoped coercion primitives):

| Rust type | JS input | Coercion |
|---|---|---|
| `f64`, `i32`, `u32`, `i64` | number-ish | ToNumber / ToInt32 / … (spec, may re-enter JS) |
| `bool` | any | ToBoolean (never re-enters) |
| `DOMString` (owns `String`) | any | ToString |
| `USVString` (owns `String`) | any | ToString + lone-surrogate scrub |
| `Option<T>` | absent / `undefined` | `None`; else inner conversion |
| `Sequence<T>` (owns `Vec<T>`) | any iterable | drives `Symbol.iterator` protocol (§6.8), each element via `T: FromJs` |
| `BufferSource` (owns `Vec<u8>`) | ArrayBuffer / any typed-array view | bytes copied out at conversion time (detach-safe; matches Blob/crypto semantics) |
| `HostRef<'s, T>` | branded host instance | brand check incl. ancestry (§3.4); TypeError with class name otherwise |
| `JsValue<'s>` (= `Scoped<'s>`) | anything | identity — the explicit "give me the raw handle" escape |
| `Callback<'s>` | callable | callable check; invokable via `MarshalCx` |

Derives:

```rust
/// WebIDL dictionary. Absent/undefined member → default; `required` errors.
#[derive(FromJs, Default)]
#[js(dictionary)]
pub struct BlobPropertyBag {
    #[js(name = "type", default)]
    content_type: USVString,
}

/// WebIDL union. Variants tried in declaration order using each variant's
/// distinguishing check (host brand → buffer → fallthrough coercion last).
#[derive(FromJs)]
#[js(union)]
pub enum BlobPart {
    Blob(HostRef<'static, Blob>),   // brand-distinguished  (see §7 for real lifetime)
    Buffer(BufferSource),           // ArrayBuffer/view-distinguished
    Text(USVString),                // catch-all coercion — must be last
}
```

Conversion order for a full argument list is left-to-right, one argument fully
converted before the next starts, matching WebIDL. Because coercions can run
user JS (a `valueOf` that detaches a buffer), `BufferSource` copies its bytes
*at its own conversion step* — later coercions cannot invalidate it.

### 2.3 `IntoJs` — return-value construction

```rust
pub trait IntoJs {
    fn into_js<'s>(self, cx: &mut MarshalCx<'_, '_, 's>) -> Result<Scoped<'s>, JsError>;
}
```

Provided impls:

| Rust type | JS result |
|---|---|
| `()`, `bool`, `f64`, integer types | `undefined` / boolean / number |
| `String`, `&str`, `DOMString`, `USVString` | string |
| `ArrayBuffer(Vec<u8>)` (marshal newtype) | `ArrayBuffer` — via §6.4 |
| `Uint8Array(Vec<u8>)` (marshal newtype) | `Uint8Array` over a fresh buffer — via §6.3 (the `Blob.prototype.bytes()` gap) |
| `Option<T>` | `null` / value (WebIDL nullable) |
| `Vec<T>` | dense array |
| `T` where `T` is a declared host class | **a real branded instance of the class** — allocated with the class's registered prototype; this is how `slice()` returns a `Blob` without any builder function |
| `JsValue<'s>` | identity |
| `#[derive(IntoJs)]` struct | plain object, fields in declaration order (`#[js(name = "...")]` per field) |

Byte-carrying returns are **explicit newtypes**, not blanket `Vec<u8>`
impls — whether bytes become an `ArrayBuffer` or a `Uint8Array` must be
visible at the declaration site (project rule: export shape obvious where
declared).

`Result<T, JsError>` is handled at the glue level, not by `IntoJs`.
Promises are not an `IntoJs` impl either — they are the `async fn` protocol
(§3.5), so "returns a promise" is visible in the signature as `async`.

### 2.4 GC safety by construction

Generated glue for every method/constructor/accessor has one shape:

```text
ctx.scope(|ctx, s| {
    let mut mcx = MarshalCx::new(ctx, s);
    // 1. receiver brand check → &T / &mut T (or owned snapshot for async)
    // 2. FromJs each argument, left to right   (all handles live in `s`)
    // 3. call the user fn — sees only Rust data + Scoped handles
    // 4. IntoJs the result inside the same scope
    Ok(ctx.escape(result))
})
```

User code never holds a raw `Value`; every intermediate is either plain Rust
data or a `Scoped<'s>` parked in the traced arena. The URL-style "snapshot
fields before moving state in" dance disappears because the glue converts the
*returned Rust value* after the body completes — there is no window where a
minted JS value coexists with un-rooted locals. `OTTER_GC_STRESS=1..16` runs
over every derive/impl are part of the trait layer's own test suite, not each
binding's.

### 2.5 `JsError`

One error type for the whole layer, constructible without a ctx:

```rust
pub enum JsError {
    Type(String),
    Range(String),
    Dom { name: &'static str, message: String },   // e.g. "NotSupportedError"
    Thrown(PersistedValue),                        // re-throw an existing JS value
}
```

Glue prefixes the operation name automatically ("Blob.prototype.slice: …"),
killing every hand-threaded `const NAME: &str` + `crate::type_error(NAME, …)`
pair. `Dom` maps to a real `DOMException` instance once the class is declared
natively (until then, to the shim's).

---

## 3. Host classes: `#[couch]` on an impl block

The declaration form moves from a spec-table macro (`couch! { ... }`) to an
attribute macro over an ordinary `impl` block, because the *signatures* are
now the descriptor: parameter types declare extraction, return types declare
construction, `async` declares the promise protocol. JS names stay explicit —
the macro never infers a JS name from a Rust identifier. WebIDL `length` is
derived from the count of non-`Option` parameters (the WebIDL rule), with
`length = N` available as an override.

### 3.1 Declaration shape

```rust
pub struct Blob { bytes: Arc<[u8]>, content_type: String }   // plain Rust data

#[couch(name = "Blob", feature = WEB)]
impl Blob {
    #[constructor]
    fn new(parts: Option<Sequence<BlobPart>>, options: Option<BlobPropertyBag>)
        -> Result<Blob, JsError> { ... }

    #[getter(name = "size")]
    fn size(&self) -> f64 { ... }

    #[getter(name = "type")]
    fn content_type(&self) -> &str { ... }

    #[method(name = "slice", length = 2)]
    fn slice(&self, start: Option<f64>, end: Option<f64>, ct: Option<USVString>) -> Blob { ... }

    #[method(name = "arrayBuffer")]
    async fn array_buffer(self) -> Result<ArrayBuffer, JsError> { ... }
}
```

Expansion target: the same static `ClassSpec`/`MethodSpec`/installer machinery
`couch!` emits today (no new runtime registration model), plus per-member glue
fns of the §2.4 shape, plus a `ClassDescriptor` (§3.3). `feature`, statics,
constants, `no_prototype` etc. carry over as attribute arguments with the same
meanings.

`[Symbol.toStringTag]` defaults to `name` on the prototype (WebIDL rule);
`tag = false` opts out for spec outliers. Symbol-keyed methods:
`#[method(symbol = iterator)]` (needed for Headers/URLSearchParams/FormData
migration).

A class may attach a co-located JS file — `js = "blob.class.js"` — for the
members that are genuinely better written in JS (see §5.1 for the install
contract):

```rust
#[couch(name = "Blob", feature = WEB, js = "blob.class.js")]
impl Blob { ... }
```

```js
// blob.class.js — runs right after the native class installs, same unit.
// `stream()` composes ReadableStream; pointless to hand-write in Rust.
Blob.prototype.stream = function stream() {
  const bytes = this;                       // native methods already present
  return new ReadableStream({
    async start(controller) {
      controller.enqueue(await bytes.bytes());
      controller.close();
    },
  });
};
```

### 3.2 Constructor protocol — the prototype-linkage fix

The root cause of pain #1 is that native constructors *build and return their
own instance*, bypassing `OrdinaryCreateFromConstructor`. The fix inverts
control: **the VM allocates the instance; the native body returns only the
host data.**

New-expression flow for a declared class (`[[Construct]]` implemented in
`otter-vm`, once, for all host classes):

1. `proto` = `new.target.prototype` if it is an object, else the class's
   registered default prototype (spec `GetPrototypeFromConstructor` with the
   realm fallback).
2. Allocate the host object **with that prototype**, brand slot empty.
3. Run the generated constructor glue with the instance as `this`:
   extract args → call the user `fn new(...) -> Result<T, JsError>` → store
   `T` into the instance's host slot, stamping the brand (`ClassId`).
4. Return the instance. The native body cannot substitute another object —
   WebIDL constructors never need to, and the cases that look like they do
   (`URL.parse` returning `null`) are statics.

Consequences, all free at the declaration site:

- `instanceof` correct, no `link_class_prototype`, no name-based
  `class_instance_prototype` lookup (which breaks if the global is shadowed).
- **JS subclasses just work**: `class Foo extends Blob { … super(parts) … }` —
  `super()` reaches Blob's `[[Construct]]` with `new.target === Foo`, so the
  instance is born with `Foo.prototype`. The `Object.setPrototypeOf` re-homing
  in `web_bootstrap.js` is deleted.
- Rust-side construction gets the same path: `T: IntoJs` for declared classes
  allocates via the registry prototype — `slice()` returning `Blob` is one
  line, `blob_object()` is deleted.

`callable_only`, `is_abstract` keep their `couch!` meanings.

### 3.3 Class registry (per isolate — no globals)

A `ClassRegistry` field on `Interpreter`:

```rust
struct RegisteredClass {
    name: &'static str,
    brand: core::any::TypeId,        // host-data type
    parent: Option<ClassId>,         // native inheritance chain
    prototype: PersistentRootId,     // survives moving GC
    constructor: PersistentRootId,
}
```

`ClassId` is resolved from the static `ClassDescriptor`'s address at install
time and cached in the descriptor's per-isolate slot on the registry — no
`thread_local!`, no process-global map. `NativeCtx` gains
`class_prototype(ClassId)` / `construct_instance(ClassId, T)` (§6.2); the
name-string lookup `class_instance_prototype(&str)` is deleted after
migration.

### 3.4 Receivers, branding, native inheritance

- `&self` / `&mut self` → glue does a brand-checked host-data borrow with the
  spec TypeError on mismatch ("Blob.prototype.slice called on an object that
  is not a Blob"). Replaces `runtime_this_object` + `runtime_with_host_data`
  + per-class `*_receiver`/`*_snapshot` helpers.
- `self` (owned) → allowed only on `async` methods and requires `Self: Clone`;
  the glue snapshots the host data synchronously before the future is built
  (§3.5).
- Native subclassing:

```rust
pub struct File { #[js(parent)] blob: Blob, name: String, last_modified: f64 }

#[couch(name = "File", feature = WEB, extends = Blob)]
impl File { ... }
```

  `extends = Blob` + the `#[js(parent)]` field make the macro emit the
  ancestry hook: a brand check for `Blob` on a `File` instance resolves
  through the parent field, so every inherited `Blob` method works on `File`
  unmodified. The registry links `File.prototype → Blob.prototype` and
  `File.__proto__ → Blob` at install. The whole JS `File` shim (§`web_bootstrap.js`)
  is deleted.

### 3.5 Async methods

`async fn` in a `#[couch]` impl compiles to the promise protocol. The type
system enforces the VM invariant (nothing GC-touching crosses `.await`): the
generated future captures only the extracted Rust arguments and the owned
host-data snapshot, and must be `Send + 'static`. A `Scoped`/`MarshalCx`/
`NativeCtx` capture is a compile error, exactly like a handle escaping its
scope today.

Sync phase (has ctx): brand-check + snapshot receiver → `FromJs` args →
mint pending promise + `PromiseCompleter` (§6.6) → build future → return
promise `Value`.

Completion: the future resolves to `Result<R, JsError>` where `R: IntoJs +
Send`. The completer posts a completion job to the isolate's host-event queue
(the same checkpoint that drains timers/serve events); the job runs on the
mutator turn, opens a scope, converts `R` via `IntoJs`, settles the promise.

Fast path — this must not force a Tokio hop for data-only methods like
`Blob.arrayBuffer`: the glue polls the future once synchronously; if it is
immediately `Ready`, it settles through the existing fulfilled/rejected-promise
path with no executor round-trip (spec-observable ordering is unchanged —
reactions still run as microtasks). A body with no real `.await` costs what
today's manual `fulfilled_promise_with_roots` costs.

Spawning (when genuinely pending) goes through the isolate's Tokio handle —
the same plumbing `#[dive(deep)]` uses today, generalized into the completer
(§6.6) so `#[dive(deep)]`, `lodge!` async exports, and class methods share one
implementation.

Cancellation/abort: **not** auto-plumbed (non-goal, §9). If the isolate shuts
down, the completer's queue is gone and the completion is dropped; the promise
never settles, which is the correct terminal behavior.

### 3.6 Capabilities

Boolean gates are declarative:

```rust
#[method(name = "query", cap = net)]        // glue: capability check before body
async fn query(self, input: DOMString) -> Result<...> { ... }
```

The glue emits the check at the boundary, before argument coercion side
effects. Argument-*derived* checks (fs path allowlists against the actual
path argument) stay in the body, taken from an injected `caps: Caps<'_>`
parameter (the `lodge!(capabilities = true)` snapshot, typed) — per the
"keep code manual when capability sequencing matters" rule, the framework
provides the snapshot, never guesses the check.

### 3.7 Escape hatch: `raw`

```rust
#[method(name = "internalFastPath", raw)]
fn fast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { ... }
```

Registers through the identical `MethodSpec` path with no glue. This is the
guarantee that the abstraction never *costs* anything it didn't buy: hot
surfaces (the serve request path, fetch internals) keep their hand-tuned
bodies while still living inside the declared class — same prototype, same
registry, same install unit.

---

## 4. Namespaces, functions, modules

Same trait layer, three thinner declaration forms.

**Namespaces** (`crypto`, `performance`, `navigator`) — `#[holt]` on an impl
of a unit/marker type; methods are static (no receiver, no brand):

```rust
pub struct Crypto;

#[holt(name = "crypto", feature = WEB)]
impl Crypto {
    #[method(name = "randomUUID")]
    fn random_uuid() -> Result<DOMString, JsError> { ... }

    #[method(name = "getRandomValues")]
    fn get_random_values<'s>(view: JsValue<'s>, cx: &mut MarshalCx<'_, '_, 's>)
        -> Result<JsValue<'s>, JsError> { ... }   // in-place mutation keeps the handle form

    #[namespace(name = "subtle")]                  // nested namespace
    mod subtle { #[method(name = "digest")] async fn digest(...) -> ... }
}
```

This collapses the current `crypto` split (hidden `__otterCrypto*` native
globals + JS shim doing validation) into one declaration: validation is the
`FromJs` types, the DOMException names are `JsError::Dom`, and the hidden
globals disappear.

**Standalone function globals** — `#[dive]` gains typed signatures (same glue
generator); `#[dive(deep)]` becomes the same `async fn` protocol as §3.5:

```rust
#[dive(global, name = "atob", length = 1)]
fn atob(data: DOMString) -> Result<DOMString, JsError> { ... }
```

**Hosted modules** — `lodge!` keeps its shape; export rows may now point at
typed `#[dive]` fns, so `otter:kv`/`otter:sql`/`otter:ffi` migrate export-by-
export with no loader change. A lodge may also export declared classes:
`classes = [KvStore]` installs the constructor as a module export instead of
a global (registry entry is identical). And a lodge may attach its JS half —
the existing `fs_ext.rs` + `fs.js` file convention, made declarative:

```rust
lodge! {
    prefix = "otter",
    name = "fs",
    capabilities = true,
    exports = { "readFileSync" / 1 => read_file_sync, ... },   // native half
    js = "fs.js",                                              // JS half
}
```

The JS half is an ordinary module source evaluated with the native exports
already bound (importable under the same specifier); its own exports are
merged into the module namespace. Native and JS halves install as one unit —
no state where one exists without the other (§5.1).

---

## 5. Extension bundles: `romp!` (install + laziness)

One declaration per extension replaces the hand-maintained registry, the
lockstep test, the eval-built accessor shim, and the per-global one-off
installers:

```rust
romp! {
    name = "web",
    install = lazy,                      // default for globals
    classes    = [blob::Blob, blob::File, url::Url /* … */],
    namespaces = [crypto::Crypto, performance::Performance],
    functions  = [globals::atob, globals::btoa, globals::structured_clone],
    eager      = [globals::self_global, globals::navigator],   // exceptions declared, not choreographed
    js         = [include_str!("web_streams.js"), include_str!("web_fetch.js")],
}
```

Emits `pub static WEB_EXTENSION: Extension` (static descriptor — same
zero-runtime-registration philosophy as today's macros), consumed by
`Runtime::install_extension(&WEB_EXTENSION)`.

Semantics:

- **Name list derived, not maintained.** Every global name the extension
  defines is known from the declarations at compile time; `WEB_GLOBAL_NAMES`
  and its drift test are deleted. JS sources still get scanned for `def('…')`
  at build/test time only until they are fully migrated (§8), then that too
  dies.
- **Lazy is native.** `install = lazy` registers one native lazy accessor per
  name (§6.7) — no string-built `(0, eval)` shim. First touch of *any* name
  materializes the *whole extension in declaration order*: native classes,
  namespaces, functions, then the `js` sources. That ordering guarantee kills
  pain #4: a JS-glued member can never be missing next to its native class,
  because they are one unit — there is no state in which the native half
  exists and the JS half doesn't.
- **Eager exceptions are declared** (`self` must pre-exist any Web touch),
  not implemented as bespoke `install_self` eval strings — an eager function
  global is just installed at `install_extension` time.
- `js` sources run in order, after natives, exactly once — the current
  concatenation-with-separators logic becomes framework behavior.

Interim state is first-class: an extension whose classes are still shim-JS
just lists more `js` sources and fewer `classes`; migration moves names from
one list to the other one class at a time.

### 5.1 Attached JS files (per-declaration glue)

Any declaration — class (`#[couch(js = "...")]`), namespace
(`#[holt(js = "...")]`), hosted module (`lodge!(js = "...")`) — may attach
one JS source file next to its Rust file (the `module_ext.rs` + `module.js`
convention, generalized). This is the designed home for the "part of this
class is genuinely better in JS" case that today lands in a distant
`web_bootstrap.js` with a load-ordering hazard.

Contract:

1. **Path** resolves relative to the declaring Rust file (compile-time
   `include_str!` semantics); the source ships inside the static
   `Extension` descriptor like everything else.
2. **Order**: the attached source evaluates immediately after its owner's
   native install, before the next member of the extension — natives first,
   glue second, atomically in the same materialization unit. A JS-defined
   `Blob.prototype.stream` can never be missing next to a live native `Blob`;
   pain #4 stays dead even for mixed classes.
3. **Scope**: for classes/namespaces the source runs in global scope after
   the owner binding exists (augment via `Name.prototype.x = ...` /
   `Object.defineProperty`); for lodges it is module source with the native
   exports pre-bound and its exports merged into the namespace.
4. **Names still derived**: a global name defined by attached JS is declared
   in the attribute (`js_defines = ["..."]`) if it adds globals beyond its
   owner — the framework keeps the lazy-accessor list complete without a
   hand list; omitting it is a build-time error caught by the extension's
   def-scan test until the scan itself is retired.
5. **Same tiers**: embedder extensions attach JS files identically — an
   embedder can ship a Rust core + JS sugar module with zero extra install
   code.

Extension-level `js = [...]` (whole-file shim sources) remains for the
migration interim and for pure-JS members (streams today); per-declaration
attachment is the end-state for mixed native/JS surfaces.

---

## 6. VM-surface additions (each justified by a pain)

All on `otter-vm` unless noted. Everything scoped; nothing hands back a raw
`Value` except `escape`.

| # | Addition | Justification |
|---|---|---|
| 6.1 | **Host-class `[[Construct]]`**: `GetPrototypeFromConstructor`-honoring construct path that pre-allocates the branded host object and runs the native ctor glue against it (§3.2) | Pain 1 — the root fix; deletes `link_class_prototype`, `class_instance_prototype(&str)`, JS `setPrototypeOf` re-homing |
| 6.2 | **Per-isolate `ClassRegistry`** + `NativeCtx::{class_prototype(ClassId), construct_instance(ClassId, T) -> Scoped}` (§3.3). The prototype/constructor roots are keyed `ClassId × RealmId`, and CORE-tier install writes the prototype into `realm_intrinsics` (%Map.prototype%-style slots) — see §10 | Pains 1, 5 — prototype identity without name lookups; `IntoJs` for host classes; correct extra-realm (`$262.createRealm`) behavior; no thread-locals |
| 6.3 | `scoped_typed_array_from_bytes(s, TypedArrayKind, Vec<u8>) -> Scoped` (+ `scoped_uint8array_from_bytes` convenience) | Pain 2 — the missing builder that forced dropping `Blob.prototype.bytes()`; also needed by crypto `getRandomValues` migration |
| 6.4 | `scoped_array_buffer_from_bytes(s, Vec<u8>) -> Scoped` | Pain 2 — scoped form of `array_buffer_from_bytes_rooted`; uniform with the rest of the scope API |
| 6.5 | `scoped_promise_fulfilled(s, Scoped) -> Scoped` / `scoped_promise_rejected(s, Scoped) -> Scoped` | Pain 2/6 — replaces `fulfilled_promise_with_roots` + `Value::promise` hand-assembly; the async fast path (§3.5) lands on these |
| 6.6 | **Pending promise + completer**: `scoped_promise_pending(s) -> (Scoped, PromiseCompleter)`; `PromiseCompleter: Send` carries `Result<R: IntoJs + Send, JsError>` to the isolate's host-event queue; conversion + settle run on the mutator turn (generalizes the `#[dive(deep)]` plumbing into one shared implementation) | Pain 6 — async methods, `lodge!` async exports, and `dive(deep)` share one production path |
| 6.7 | **Native lazy-global installer** (`otter-runtime`): `Runtime::install_lazy_globals(names, materializer)` — native accessor per name; first get runs the materializer once, redefines data properties | Pain 5 — deletes the string-built eval shim and its re-entrancy footguns |
| 6.8 | `scoped_iterate(s, v, |cx, elem: Scoped| -> Result<(), JsError>)` — drives the `Symbol.iterator` protocol with re-entry | Pain 3 — `Sequence<T>` must accept any iterable per WebIDL, not just arrays (today's `as_array` path is wrong for generators/Sets) |
| 6.9 | Scoped spec coercions: `scoped_to_string / scoped_to_usv_string / scoped_to_number / scoped_to_boolean` (re-entrant ToPrimitive under the scope contract) | Pain 3 — `FromJs` primitive impls need spec coercions that survive `valueOf` re-entry |
| 6.10 | Brand-checked host data: `scoped_host_data::<T>(v) -> Result<&T, JsError>` / `_mut`, walking the registry ancestry chain (§3.4) | Pains 1, 7 — replaces `runtime_this_object` + `runtime_with_host_data` + per-class snapshot helpers; makes `File`-inherits-`Blob`-methods sound |
| 6.11 | `scoped_define_accessor(s, obj, key, getter, setter, flags)` + symbol-keyed method / `Symbol.toStringTag` rows in `ClassSpec` install | Pains 1, 4 — prototype accessors and the toStringTag WebIDL default from the declaration; URL live accessors (replacing its snapshot data properties) need the runtime-path variant |
| 6.12 | `JsError` (§2.5) + `vm_to_native` mapping with automatic operation-name prefixing; DOMException construction routed through the registry once DOMException is a declared class | Pains 2, 3 — one error channel; kills per-fn `NAME` threading |
| 6.13 | **Spec receiver coercers** exposed to glue: `ThisNumberValue` / `ThisStringValue` / `ThisBooleanValue` / internal-slot kind checks (`Value::map` / `Value::set` / typed-array kind), so `#[method(this = number)]`-style receivers on primitive-wrapper prototypes compile to the exact abstract op | §10 — builtin tier: `Number.prototype.*` / `Map.prototype.*` receivers without hand-written kind checks |

Explicitly **not** added: any process-global or thread-local registry (per
CLAUDE.md and the multi-isolate rule); any env-var/feature toggle for the new
path (it becomes *the* path per surface as it migrates, revert = git); any
dependency from active crates into parked shims.

---

## 7. Worked exemplar: Blob + File

### Before (reality today)

- `blob.rs`: 239 lines — `couch!` table + 9 free fns (`blob_constructor_native`,
  `assemble_blob_parts`, `append_blob_part` with three hand-rolled BufferSource
  branches, `blob_options_type`, `blob_receiver`/`blob_snapshot`/`host_error`
  plumbing, `blob_object` + `link_class_prototype` call), manual
  ArrayBuffer+promise assembly in `arrayBuffer`, **no `bytes()` at all**
  (missing typed-array builder).
- `web_bootstrap.js`: a 30-line JS `File extends Blob` shim doing
  `Object.setPrototypeOf(this, new.target.prototype)` and own-property
  `name`/`lastModified` defines, plus `tagged(File.prototype, 'File')`, plus
  a `def('File', …)` that must stay in lockstep with `WEB_GLOBAL_NAMES`.

### After (complete — this is the whole surface)

```rust
//! WHATWG Blob / File.

use otter_runtime::marshal::{
    ArrayBuffer, BufferSource, HostRef, JsError, Sequence, Uint8Array, USVString,
};
use std::sync::Arc;

#[derive(Clone)]
pub struct Blob {
    bytes: Arc<[u8]>,          // Arc: async snapshots are O(1), no VM handles held
    content_type: String,
}

#[derive(FromJs, Default)]
#[js(dictionary)]
pub struct BlobPropertyBag {
    #[js(name = "type", default)]
    content_type: USVString,
}

#[derive(FromJs)]
#[js(union)]
pub enum BlobPart<'s> {
    Blob(HostRef<'s, Blob>),
    Buffer(BufferSource),
    Text(USVString),
}

#[couch(name = "Blob", feature = WEB)]
impl Blob {
    #[constructor]  // length = 0 per spec: both params optional
    fn new(parts: Option<Sequence<BlobPart<'_>>>, options: Option<BlobPropertyBag>)
        -> Result<Blob, JsError>
    {
        let mut bytes = Vec::new();
        for part in parts.into_iter().flatten() {
            match part {
                BlobPart::Blob(b) => bytes.extend_from_slice(&b.bytes),
                BlobPart::Buffer(buf) => bytes.extend_from_slice(buf.as_ref()),
                BlobPart::Text(text) => bytes.extend_from_slice(text.as_str().as_bytes()),
            }
        }
        let content_type = options.unwrap_or_default().content_type;
        Ok(Blob { bytes: bytes.into(), content_type: normalize_type(content_type.as_str()) })
    }

    #[getter(name = "size")]
    fn size(&self) -> f64 { self.bytes.len() as f64 }

    #[getter(name = "type")]
    fn content_type(&self) -> &str { &self.content_type }

    #[method(name = "slice", length = 2)]
    fn slice(&self, start: Option<f64>, end: Option<f64>, ct: Option<USVString>) -> Blob {
        let (start, end) = clamp_range(self.bytes.len(), start, end);   // spec §3, incl. negatives
        Blob {
            bytes: self.bytes[start..end].into(),
            content_type: ct.map_or_else(|| self.content_type.clone(),
                                         |t| normalize_type(t.as_str())),
        }
    }

    #[method(name = "arrayBuffer")]
    async fn array_buffer(self) -> Result<ArrayBuffer, JsError> {
        Ok(ArrayBuffer(self.bytes.to_vec()))
    }

    #[method(name = "bytes")]                       // previously inexpressible
    async fn bytes(self) -> Result<Uint8Array, JsError> {
        Ok(Uint8Array(self.bytes.to_vec()))
    }

    #[method(name = "text")]
    async fn text(self) -> Result<USVString, JsError> {
        Ok(USVString::from(String::from_utf8_lossy(&self.bytes).into_owned()))
    }
}

#[derive(Clone, FromJs, Default)]
#[js(dictionary)]
pub struct FilePropertyBag {
    #[js(name = "type", default)]         content_type: USVString,
    #[js(name = "lastModified", default)] last_modified: Option<f64>,
}

pub struct File {
    #[js(parent)]
    blob: Blob,
    name: String,
    last_modified: f64,
}

#[couch(name = "File", feature = WEB, extends = Blob)]
impl File {
    #[constructor]  // length = 2: fileBits, fileName required
    fn new(bits: Sequence<BlobPart<'_>>, name: USVString, options: Option<FilePropertyBag>)
        -> Result<File, JsError>
    {
        let options = options.unwrap_or_default();
        let blob = Blob::new(Some(bits),
                             Some(BlobPropertyBag { content_type: options.content_type }))?;
        Ok(File {
            blob,
            name: name.into_string(),
            last_modified: options.last_modified.unwrap_or_else(now_ms),
        })
    }

    #[getter(name = "name")]
    fn name(&self) -> &str { &self.name }

    #[getter(name = "lastModified")]
    fn last_modified(&self) -> f64 { self.last_modified }
}
```

Registration is two entries in `romp! { classes = [blob::Blob, blob::File] }`.

What the framework now owns that the old code did by hand or couldn't do:
prototype linkage + `instanceof` (incl. `class Foo extends Blob` in user JS,
via §3.2), `[Symbol.toStringTag]` on both prototypes, brand checks with spec
error messages, BufferSource/dictionary/sequence/union extraction with WebIDL
coercion order, `Promise<ArrayBuffer>` / **`Promise<Uint8Array>`** /
`Promise<USVString>` returns, `slice()` returning a real branded `Blob`,
`File` inheriting every `Blob` member natively (JS shim deleted), lazy
install with zero registry maintenance. Every JS name, arity, and export
shape is still literally visible in the declaration.

---

## 8. Migration path

Incremental, one surface at a time, each phase landing green. Gates for every
phase: `cargo test --all`, `cargo test -p otter-web`, test262 **failing-set
diff** vs stashed baseline (not just pass-rate), `OTTER_GC_STRESS=1..16`
identical-output runs over the migrated surface, `cargo clippy --all-targets
--all-features -- -D warnings`, serve/bench sanity where the surface touches
hot paths.

- **P0 — marshalling core** (`otter-vm` + `otter-runtime` re-exports).
  `MarshalCx`, `FromJs`/`IntoJs` + primitive impls, `JsError`, derives,
  scoped builders 6.3/6.4/6.5, coercions 6.9, iteration 6.8, host-data 6.10.
  Pure addition; nothing migrates yet. Own GC-stress + trybuild suite
  (handle escape, `.await` capture = compile errors).
- **P1 — constructor protocol + registry** (6.1, 6.2, 6.11). Retrofit the
  *existing* `couch!` install path onto the registry so current classes get
  correct `[[Construct]]`/prototype behavior immediately; delete
  `link_class_prototype`, the JS `setPrototypeOf` re-homing, and
  `class_instance_prototype(&str)`. This fixes pain #1 for every class
  before any class is rewritten.
- **P2 — `#[couch]` v2 + Blob/File exemplar.** Rewrite Blob per §7, File
  native, delete the File shim from `web_bootstrap.js`. `bytes()` ships.
  This is the proof gate: if the exemplar needs any hand-rolled escape
  beyond `raw`, the design failed and gets revised here, before spreading.
- **P3 — async protocol** (6.6): completer, `async fn` methods,
  `#[dive(deep)]` rebased onto it. Blob's promise methods move from the
  fast path to the full protocol test coverage.
- **P4 — `romp!` + native lazy install** (6.7). `web` extension declared;
  `WEB_GLOBAL_NAMES`, the lockstep test, the eval accessor shim,
  `install_navigator`/`install_self` one-offs all deleted. JS shim sources
  become `js = […]` members (unchanged content).
- **P5 — surface-by-surface migration**, each its own commit with gates:
  URL (live accessors via 6.11 replace snapshot data props), crypto
  (`#[holt]`, hidden `__otterCrypto*` globals deleted), atob/btoa/
  structuredClone (`#[dive]` typed), then the big fetch classes
  (Headers → Request → Response) out of `web_fetch.js` — keeping their
  serve-hot internals `raw` until profiles say the glue is free.
- **P6 — modules**: `lodge!` typed exports; `otter:kv`/`otter:sql`/`otter:ffi`
  export-by-export. Old declaration forms (`couch!` table syntax, `holt!`
  table syntax) deleted when the last user is gone — no compat shims kept.
- **P7 — embedder tier + builtins convergence.** Publish the public extension
  API from `otter-runtime` (§11) with the "Writing an extension" guide and a
  template crate; re-declare the in-tree `Otter` global/module through it as
  the dogfood proof. CORE builtins migrate to the v2 declaration form
  **opportunistically** — a builtin moves when it is next touched, keeping
  its hot bodies `raw` (§10); no big-bang rewrite of `otter-vm` intrinsics.

Rollback story per project rules: no flags; a phase that regresses is
reverted in git.

---

## 9. Risks and non-goals

**Risks**

- *Coercion re-entrancy.* `ToNumber`/`ToString` can run arbitrary JS (detach
  buffers, mutate the receiver). Mitigations are structural: strict
  left-to-right conversion, `BufferSource` copies at its own conversion step,
  receiver host-data is borrowed only after all coercions (or snapshotted
  first for async). This must be tested with adversarial `valueOf` fixtures,
  not assumed.
- *Hidden copies.* `BufferSource` owning `Vec<u8>` is a copy by design
  (matches Blob/digest semantics), but it would be the wrong default for a
  future zero-copy surface (streams chunks). The design keeps `JsValue<'s>` +
  `raw` for those; the risk is contributors reaching for `BufferSource` out
  of habit on hot paths — the docs must state the cost, and serve/fetch stay
  `raw` until profiled.
- *Glue cost on hot calls.* Scope open + per-arg dispatch is fine for Web-API
  frequency, unproven for per-request paths. Guard: P2 exemplar gets a
  microbench vs the hand-written version; serve internals migrate last and
  only behind flat profiles.
- *Async executor coupling.* The completer assumes the isolate's host-event
  queue outlives the future or drops it safely. Isolate teardown with
  in-flight futures needs an explicit test (completer resolves into a dead
  queue → silent drop, no UB, no leak).
- *Macro surface creep.* An attribute macro over impl blocks can grow
  unbounded options. Rule: anything not expressible as signature + one
  attribute row goes to `raw` or stays manual — the macro is a descriptor,
  not a DSL.
- *`length`/name drift from spec.* Deriving `length` from non-`Option` params
  is the WebIDL rule but reviewers must be able to check it at the
  declaration; WPT/test262 function-length assertions are the backstop.

**Non-goals**

- Automatic AbortSignal/cancellation plumbing for async methods (manual,
  per-surface).
- Zero-copy `BufferSource` borrows across user-code boundaries (unsound under
  moving GC + re-entrancy; explicit handles only).
- Migrating capability *sequencing* into attributes — only boolean gates are
  declarative; path/host-allowlist checks against argument values stay in
  bodies with the injected snapshot.
- ABI stability for embedders — the embedder tier (§11) is **source-level**
  stable under semver; no C-ABI / dylib plugin story in this design.
- Rewriting `web_streams.js`/`web_fetch.js` logic in Rust as part of this
  work — they migrate *as JS members* of the extension first (pain 4/5
  fixed), native rewrites are separate, profile-driven efforts.
- Touching parked crates (`otter-nodejs`, `otter-node-compat`).

---

## 10. Spec builtins (`feature = CORE`) on the same layer

`couch!`/`holt!` already declare core intrinsics today (`Proxy`, `Number`,
TypedArrays, `String.fromCharCode` static specs). The v2 forms keep that role
— the declaration layer runs from `Map` to `Blob` to an embedder's `Db` with
one syntax. Builtins differ from platform classes in four ways, and each
difference is a declaration option, not a separate system:

### 10.1 Backing data is VM-native, not host data

A `Map` instance's internal slots are the VM's `JsMap`, not a `HostObjectData`
box; `Number.prototype` methods take a primitive-coerced receiver, not a
branded object at all. The class data model generalizes:

```rust
#[couch(name = "Map", feature = CORE, data = intrinsic(JsMap))]
impl MapClass {
    #[constructor]
    fn new(entries: Option<JsValue<'_>>, cx: &mut MarshalCx<'_, '_, '_>) -> ... { ... }

    #[method(name = "get", raw)]                 // IC/JIT fast path — untouched body
    fn get(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> { ... }

    #[method(name = "groupBy", static_)]         // cold static — typed glue is fine
    fn group_by(items: JsValue<'_>, callback: Callback<'_>, ...) -> ... { ... }
}
```

- `data = intrinsic(JsMap)` — `&self`-style receivers bind a scoped,
  non-owning `JsMapRef<'s>` wrapper; the brand check is the value-kind check
  (`Value::map`), not a `TypeId` lookup. Same for `Set`, ArrayBuffer,
  typed arrays, Date.
- `data = primitive(number)` (or per-method `#[method(this = number)]`) —
  receiver runs the exact spec abstract op (`ThisNumberValue` etc., §6.13)
  and hands the body an `f64`. `callable_only` / `is_abstract` /
  `no_prototype` / `prototype_constants` / `static_constants` carry over
  unchanged.

### 10.2 Realm intrinsics

CORE prototypes/constructors must land in `realm_intrinsics`
(%Map.prototype%, %Number.prototype%) and exist **per realm**
(`$262.createRealm`). This is why the `ClassRegistry` (§3.3, 6.2) keys its
roots `ClassId × RealmId` and why install for `feature = CORE` also binds the
named intrinsic slot. Platform and embedder classes ride the same per-realm
table for free — a second realm touching `Blob` gets that realm's prototype
instead of today's single-realm assumption.

### 10.3 Fast paths must not regress — `raw` is first-class here

The interpreter's `Op::CallMethod` intrinsic dispatch consumes static
`&[MethodSpec]` slices, and the JIT method-inline path reads builtin-tagged
statics. The v2 attribute macro emits the **same static spec tables** as the
old table macros (that is the expansion target, §3.1), so those slice
addresses stay linkable and nothing about IC/JIT dispatch changes. Rule of
thumb baked into review guidance:

- Anything on an IC/JIT fast path (`Map.prototype.get/set`, `Array` hot
  methods, `String` hot methods) is declared with `raw` bodies — the
  declaration buys install/prototype/registry uniformity only, zero glue in
  the call path.
- Cold surfaces (`Number.parseFloat`, `Map.groupBy`, `Array.fromAsync`
  scaffolding, annex-B stragglers) may use typed signatures freely.
- The P2 microbench gate (§9) applies doubly here: a builtin migrates only
  behind a flat interpreter/JIT bench profile.

### 10.4 What builtins buy from the migration

Uniform install (no bespoke bootstrap glue per class), per-realm correctness
by construction, `Symbol.toStringTag`/accessor/symbol-method declarations in
one place, and — for new spec proposals — the ability to land a full builtin
(e.g. a Stage-4 `Map` addition) as one typed method instead of a hand-rooted
native. Migration is opportunistic per P7: a builtin converts when next
touched, never as a sweep.

---

## 11. Embedder tier: custom namespaces, classes, and modules

Goal: someone embedding Otter adds a top-level namespace object (an
`Otter`-style global for their product), custom host classes, and their own
`myapp:*` hosted modules — in their crate, with no VM knowledge, a few lines
per surface, capability-gated by default.

### 11.1 Public surface

`otter-runtime` re-exports the complete authoring kit, and this becomes the
supported embedder API (source-level semver stability; see non-goals for
ABI):

- Macros: `#[couch]`, `#[holt]`, `#[dive]`, `lodge!`, `romp!`, derives
  (`FromJs`, `IntoJs`, `Pelt`, `Groom`).
- Types: `MarshalCx`, marshal types (§2), `JsError`, `Extension`, `Caps`.
- Install: `Runtime::install_extension(&'static Extension)` (and the builder
  form `RuntimeBuilder::extension(...)`).

`feature = CORE/WEB` gating is an in-tree concern; the attribute is optional
and absent for embedder declarations.

### 11.2 Worked shape — a complete embedder extension

```rust
// crate: acme-runtime  (embedder's own crate, depends only on otter-runtime)
use otter_runtime::prelude::*;

pub struct Db { pool: acme_db::Pool }          // arbitrary Rust state, isolate-pinned

#[couch(name = "Db")]
impl Db {
    // No global constructor: instances come from acme.openDb() below.
    #[constructor(callable_only = true)]        // `new Db()` throws; factory-only class
    fn new() -> Result<Db, JsError> { Err(JsError::Type("use Acme.openDb()".into())) }

    #[method(name = "query", cap = net)]
    async fn query(self, sql: DOMString) -> Result<Vec<Row>, JsError> { ... }
    //             ^ Self: Clone snapshot     ^ #[derive(IntoJs)] struct → array of objects
}

#[holt(name = "Acme")]                          // top-level global namespace object
impl Acme {
    #[method(name = "version")]
    fn version() -> &'static str { env!("CARGO_PKG_VERSION") }

    #[method(name = "openDb", cap = net)]
    fn open_db(dsn: DOMString, caps: Caps<'_>) -> Result<Db, JsError> { ... }
    //                                          ^ returning a declared class mints a
    //                                            real branded instance (§2.3)
}

lodge! {                                        // module form of the same surface
    prefix = "acme",
    name = "db",
    capabilities = true,
    classes = [Db],                             // constructor exported from the module
    exports = { "open" / 1 => open_db_export },
}

romp! {
    name = "acme",
    install = eager,                            // embedder chooses; lazy works identically
    namespaces = [Acme],
    modules = [DB_HOSTED_MODULE],
}
```

```rust
// embedder init
let mut runtime = Runtime::builder()
    .with_web_apis()
    .extension(&ACME_EXTENSION)
    .build()?;
```

```js
// user script
const db = Acme.openDb("postgres://…");        // global namespace
import { open } from "acme:db";                 // or the module form
```

Adding one more namespace later = one new `#[holt]` impl + one row in the
`romp!`. Adding a class = one struct + one `#[couch]` impl + one row. That is
the whole cost model, and it is identical to the in-tree tiers — the in-tree
`Otter` global itself is re-declared through this exact path in P7 as the
dogfood proof.

### 11.3 Capabilities and safety at the embedder boundary

- `cap = …` boolean gates and the `Caps<'_>` snapshot work exactly as in-tree
  (§3.6); deny-by-default applies to embedder modules with no extra wiring —
  an embedder cannot accidentally ship an ungated network surface without
  writing `cap`-less code that never touches capability-gated `NativeCtx`
  primitives.
- Embedder-*defined* capability kinds (a custom "acme_billing" permission)
  are out of scope v1: built-in kinds only; custom policy is manual checks in
  bodies against embedder state.
- GC soundness is inherited, not re-proven per embedder: embedder code only
  ever sees Rust data, marshal types, and `Scoped` handles; the trait layer's
  own GC-stress suite (§2.4) is the guarantee. `burrow!` remains available
  for imperative host-owned object surfaces that don't fit the declarative
  model.

### 11.4 Deliverables

- "Writing an extension" guide on the docs site (mirrors the handle-scopes
  page in tone: the safe path is the only documented path).
- A template crate (`examples/extension-template/`) with one class, one
  namespace, one module, one test exercising `OTTER_GC_STRESS`.
- Compile-fail tests pinning the embedder-visible contracts (handle escape,
  `.await` capture, non-`Send` async data).
