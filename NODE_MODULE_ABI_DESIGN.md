# Native Module ABI Redesign

Goal: make writing Node (and `otter:`) builtin modules in Rust idiomatic and
safe, so ~50 Node modules can be ported without fighting the VM. Driven by
porting `assert`/`path`, which showed the current `HostedModuleCtx` is too thin.

## Prior art (Rust runtimes)
- **Deno** `deno_core`: `#[op2]` macro on Rust fns auto-marshals args/returns
  (numbers, strings, `&[u8]`, serde, `Option`, `Result`); `extension!(name, ops=[…],
  esm=[…])` declares a module + its ops; `JsRuntime` registers extensions; CJS via
  `deno_node`. Module loader unifies resolution.
- **napi-rs** `#[napi]`: trait-based conversion (`FromNapiValue`/`ToNapiValue`) so a
  Rust `fn(a: String, b: Option<u32>) -> Vec<String>` becomes a JS function; structs
  derive object marshalling.
- **Bun** (now Anthropic; Zig→Rust in progress) — JSC-based; `bun-native-plugin`
  is a bundler-plugin wrapper, not the module-export ABI, but same direction.

Takeaway: a **value-conversion trait layer** + **declarative registration** +
**unified module loader**. We adopt that, sized to our VM.

## Current pain (what we're replacing)
1. `HostedModuleCtx` only does `method`/`property(pre-built Value)` — can't build a
   string for `path.sep`, nested objects, or arrays.
2. No callable export — `assert(cond)` needed a raw-`Interpreter` escape hatch
   (`alloc_host_object` + `set_call_native` + a hand-rolled GC-root `Vec`).
3. Two divergent export paths: ESM object-namespace `install` vs CJS `cjs_value`.
4. Native bodies import `otter_vm` internals (`abstract_ops`, `object::get/set`,
   `array::len`); deep-equal / key-enumeration not exposed.
5. Manual GC rooting = UAF footgun.

## Target API

### Layer 1 — value builder + scope (auto-rooting) + unified export
```rust
pub type ModuleDefine = fn(&mut ModuleScope) -> Result<Value, ModuleError>;
// ONE installer returns ONE module.exports value. require() returns it directly;
// ESM derives default = value, named exports = own enumerable keys.

impl ModuleScope<'_> {
    fn capabilities(&self) -> &CapabilitySet;
    // value constructors — every returned Value is rooted in the scope arena
    // until the module export is built (no manual roots):
    fn string(&mut self, &str) -> Value;
    fn number(&mut self, f64) -> Value;
    fn bool(&mut self, bool) -> Value;
    fn null(&self) -> Value;  fn undefined(&self) -> Value;
    fn array(&mut self, impl IntoIterator<Item = Value>) -> Result<Value>;
    fn object(&mut self) -> ObjBuilder<'_>;          // .set(k,v).func(name,len,f).getter(...)
    fn function(&mut self, name, len, NativeFastFn) -> Result<Value>;
    fn callable(&mut self, name, len, call, build: impl FnOnce(&mut ObjBuilder)) -> Result<Value>;
}
```
`ObjBuilder` accumulates props and roots, builds a `JsObject` on `.finish()`.

### Layer 2 — conversion traits (the ergonomic win)
```rust
pub trait FromJs<'a>: Sized { fn from_js(v: Value, cx: &mut NativeCtx) -> Result<Self, NativeError>; }
pub trait ToJs { fn to_js(self, cx: &mut NativeCtx) -> Result<Value, NativeError>; }
// impls: bool, f64/i64/u32/usize, String, &str (out), Option<T>, Vec<T>, () -> undefined.
```
Native method ctx (`NativeCtx`) also gains the Layer-1 value constructors + a
curated ops surface: `strict_equal`, `loose_equal`, `same_value`, `deep_equal`,
`own_keys`, `to_string`, `to_number`, `get`, `set`. Modules never `use otter_vm`.

### Layer 3 — declarative macro (later)
```rust
otter_module! {
    name: ["node:path", "path"],
    properties: { sep: "/", delimiter: ":" },
    functions: { basename, dirname, extname, isAbsolute, join, normalize, … },
}
// `fn basename(p: String, ext: Option<String>) -> String` via FromJs/ToJs.
```
Generates the `ModuleDefine` + arg/return marshalling. Sits on existing
`raft!`/`burrow!`/`lodge!` family.

## Unifying ESM + CJS
- `HostedModule` keeps one `ModuleDefine`. `require()` returns its value.
- ESM (`module_records.rs`): build the value once; register an env that exposes
  `default` = value and named bindings = the value's own enumerable string keys
  (works for object *and* callable exports — a function is a `JsObject`).
- Removes `cjs_value`/`install` duality and the callable-export special case.

## DECISION: native-first, high-level, zero-cost ABI

Modules are implemented **natively in Rust** (faster than JS shims: no interpreter
overhead, no redundant work), but authored through a **high-level ergonomic API**
with **zero-cost value conversion** (no serde on hot paths). Bun/Deno lean on JS
shims; we go native for speed while keeping authoring easy.

### Naming — Otter lexicon only (NO Deno/Bun collisions)
No `op`/`op2`/`extension!`/`napi`/`deno_core` terms. Extend the existing Otter
macro family: `lodge!` (module home), `raft!` (grouped bindings), `burrow!`
(host objects), `dive`/`dive(deep)` (one binding), `holt!`/`couch!`. New names
stay animal-themed and ours.

### Ergonomics (easy to write)
```rust
lodge! {
    name: ["node:path", "path"],
    data:    { sep: "/", delimiter: ":" },
    methods: { basename, dirname, extname, isAbsolute, join, normalize, … },
}
// idiomatic Rust, conversions automatic (no op/op2 — these are Otter `dive` fns):
fn basename(p: Str<'_>, ext: Option<Str<'_>>) -> String { … }
fn isAbsolute(p: Str<'_>) -> bool { … }
```
`lodge!` generates the `ModuleDefine` + per-arg/return marshalling; each method is
a `dive` binding. Conversion traits are `FromJs`/`ToJs` (descriptive, not the
Deno `serde_v8` / napi `FromNapiValue` names).

### Zero-cost conversion (no perf loss)
`FromJs`/`ToJs` are thin, **not serde**:
- `f64`/`i64`/`u32`/`bool`: read the value tag directly, no boxing/alloc.
- `Str<'a>`/`&str`: borrow UTF-8/UTF-16 from the heap, no copy (copy only if the
  op needs an owned `String`).
- `Buffer`/TypedArray: borrowed `&[u8]`/`&mut [u8]`, no copy.
- objects: lazy handle (`JsObj`) with `.get`/`.set`/`.keys`, no deep serialize.
- serde-shaped marshalling is **opt-in**, only for cold/config args.
Modeled on Deno `op2` fast-calls.

### Builder + scope (auto-rooting, no UAF footgun)
Under the macro sits `ModuleScope` (Layer 1): `string/number/bool/array/object/
function/callable`, every produced `Value` rooted in the scope arena until the
export is built. Replaces the manual `roots: Vec` pattern used in `assert`.

### Curated native surface
`NativeCtx` gains `strict_equal`/`loose_equal`/`same_value`/`deep_equal`/
`own_keys`/`to_string`/`to_number`/`get`/`set` — module/op code never `use otter_vm`.

### Unified export (one path for ESM + CJS)
One `ModuleDefine -> Value`. `require()` returns it; ESM derives default = value,
named = own enumerable keys (object or callable). Removes the `install`/`cjs_value`
duality and the callable special-case.

## Rollout
1. **Layer 1** `module_scope.rs` in `otter-runtime`: `ModuleScope` + `ObjBuilder`
   + auto-rooting. Unify the export path (`commonjs.rs` builtin branch + ESM
   `module_records`) onto one `ModuleDefine`. Migrate `fs` + `assert` to it
   (delete the `cjs_value` escape hatch + the manual root Vec).
2. **Layer 2** zero-cost `FromJs`/`ToJs` + `Str`/`JsObj`/buffer borrows + curated
   `NativeCtx` ops.
3. **Layer 3** `otter_module!` macro generating registration + marshalling.
4. Port natively: `path` → `util` → `events` → `url` → `net`/`worker_threads`/
   `buffer`/`child_process` (+ `tmpdir` deps) → `common` loads → conformance
   greens. Add `process.umask` + `process.config.variables`.

`fs`/`assert` keep working throughout (new API added alongside, migrate, remove
old paths).
