# Handle-Scope Rooting Redesign

Status: ACTIVE. Owner: runtime/VM.
Goal: make it **impossible by construction** for native Rust code to hold a
stale GC handle across a moving collection, and give feature authors a simple
high-level API for building JS values — no manual `value_roots`, no re-reads.

## 1. Problem (evidence)

`Value` / `JsObject` / `JsString` are raw `Copy` cage offsets. The young
generation is a moving Cheney semispace: any allocation can relocate any young
object. Today's contract — hand-thread `value_roots: &[&Value]` /
`slice_roots: &[&[Value]]` into every allocating call and re-read every copy
afterwards — is ad-hoc and systematically violated:

- `static_call_ops.rs` `GetOwnPropertyDescriptors`: local `result` copy vs
  rooted `result_root` desync → wrote properties into a from-space corpse →
  `object.rs:869` OOB panic (`new Headers(...)` under `OTTER_GC_STRESS=8`).
  Fixed in `a999bc1f`, but only for that one site.
- `object_internal_ops.rs` `own_property_keys_value`: key strings allocated in
  a loop with earlier keys unrooted → keys silently dropped under stress.
  Fixed in `a999bc1f` with a hand-rolled rooted helper — the kind of
  boilerplate this plan eliminates.
- Still broken after both fixes: the same path truncates keys under
  `OTTER_GC_STRESS=2..8` because the receiver argument is stale **on entry**
  (upstream arg-passing hole). Per-site patching is whack-a-mole.
- Audit found the same pattern in `otter-web/src/url.rs:162` (7 unrooted
  strings), `otter-modules/src/serve.rs:698`, `otter-web/src/blob.rs:141`,
  `js_surface.rs` `ObjectBuilder` (roots only the value being stored).

Failure mechanics: a stale offset survives the scavenge that moved it (the
forwarding pointer is readable only during that scavenge); after the flip the
from-space page is recycled; the **next** scavenge sees the stale slot
pointing into `NewTo` and treats it as already-evacuated — silently
"laundering" the dangling pointer into whatever live object now occupies the
memory. Corruption surfaces far from the cause.

Reference: boa-dev/oscars research reached the same conclusion — shadow-stack
("remember to push/pop") rooting is "easy to forget, leads to bugs"; a
handle table on the context, patched once on move, won their design review.
This plan is the V8 `HandleScope`/`Local<T>` model adapted to otter.

## 2. Design

### 2.1 Core types (`crates/otter-vm/src/handles.rs`, new module)

```rust
/// Contiguous scope-handle storage. One per Interpreter. Traced (and
/// rewritten in place) by every collection, so a slot is always current.
pub struct HandleArena {
    slots: Vec<Value>,
}

impl HandleArena {
    pub(crate) fn len(&self) -> usize;
    pub(crate) fn push(&mut self, v: Value) -> u32;      // returns slot index
    pub(crate) fn get(&self, idx: u32) -> Value;
    pub(crate) fn set(&mut self, idx: u32, v: Value);
    pub(crate) fn truncate(&mut self, base: usize);
    /// Visit every live slot. Called from the runtime root walk.
    pub(crate) fn trace(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc));
}

/// Scope token. Created only by `with_handle_scope`; owns the arena range
/// `[base, arena.len())`, which is truncated when the scope exits.
pub struct HandleScope {
    base: usize,
}

/// A rooted, always-current handle. `Copy`, cheap. The `'s` lifetime pins it
/// inside the `HandleScope` that created it — it cannot escape the
/// `with_handle_scope` closure (compile error), so it can never dangle.
#[derive(Clone, Copy)]
pub struct Scoped<'s> {
    idx: u32,
    _scope: PhantomData<&'s HandleScope>,
}
```

Notes:
- `Scoped` deliberately carries **no payload**, only the arena index. Every
  read goes through the arena slot, which the collector rewrites on a move.
  There is no cached copy to go stale.
- `HandleScope` is **not** RAII (no Drop): truncation is done by the
  `with_handle_scope` wrapper so a panic unwinds through the normal wrapper
  path. Keep it private-constructible (`pub(crate)` ctor) so user code cannot
  forge one.
- Nesting works for free: inner scope's `base` = current arena len; inner
  truncate never touches outer slots. `Scoped<'outer>` stays valid inside an
  inner scope (its `'s` outlives).

### 2.2 Scope entry points

On `Interpreter`:

```rust
pub(crate) fn with_handle_scope<R>(
    &mut self,
    f: impl FnOnce(&mut Interpreter, &HandleScope) -> R,
) -> R {
    let base = self.handle_arena.len();
    let scope = HandleScope { base };
    let r = f(self, &scope);
    self.handle_arena.truncate(base);
    r
}
```

On `NativeCtx` (the surface feature authors use) — same shape:

```rust
pub fn scope<R>(&mut self, f: impl FnOnce(&mut NativeCtx<'_>, &HandleScope) -> R) -> R;
```

Borrow shape: the closure receives the ctx again (`&mut`) plus a `&HandleScope`
token. `Scoped<'s>` borrows the **token**, not the ctx, so allocating calls
(`&mut ctx`) interleave freely with live handles. This is the whole trick that
makes the API ergonomic in Rust.

Escaping a value (returning it to the VM): 

```rust
impl HandleScope {
    /// Read the current value for immediate hand-off across the scope
    /// boundary (function return to the VM, storing into an already-rooted
    /// object). The returned raw Value is valid until the next allocation:
    /// the caller must not hold it across one.
    pub fn escape(&self, ctx: &NativeCtx<'_>, v: Scoped<'_>) -> Value;
}
```

The native-call boundary immediately roots return values in the caller frame
(existing behavior), so `scope(|ctx, s| { ...; Ok(s.escape(ctx, obj)) })` is
sound.

### 2.3 Tracing wiring (the soundness core)

Two paths must see the arena — both already exist for `json_root_stack`, so
mirror them:

1. `runtime_state.rs` `trace_roots_inner` (the extra-roots provider path,
   active during dispatch): add `interp.handle_arena.trace(visitor)` next to
   the `json_root_stack_for_trace` walk (`runtime_state.rs:136`).
2. `allocation_ops.rs` `Interpreter::collect_runtime_roots` (the snapshot path
   used when **no** extra-roots provider is registered — host-side calls,
   module init): append `*mut RawGc` slots for every arena entry, same way
   the json stack is handled there. The snapshot is taken per-allocation and
   consumed within it, so `Vec` reallocation between allocations is fine.

Invariant to document in the module: **every** collection entry point that can
run while native code is on the Rust stack must trace the arena. There are
exactly two (above); a `debug_assert` in `HandleArena::get` may additionally
verify the slot holds a plausible value under `OTTER_GC_VERIFY`.

### 2.4 Scoped allocation + access API (on `NativeCtx`)

All existing rooted internals stay; these are thin wrappers that (a) allocate
via the already-rooted paths, (b) immediately park the result in the arena,
(c) hand back `Scoped`.

```rust
// creation
pub fn scoped_string<'s>(&mut self, s: &'s HandleScope, text: &str) -> Result<Scoped<'s>, NativeError>;
pub fn scoped_object<'s>(&mut self, s: &'s HandleScope) -> Result<Scoped<'s>, NativeError>;          // %Object.prototype%
pub fn scoped_object_bare<'s>(&mut self, s: &'s HandleScope) -> Result<Scoped<'s>, NativeError>;      // null proto
pub fn scoped_array<'s>(&mut self, s: &'s HandleScope, len: usize) -> Result<Scoped<'s>, NativeError>;
pub fn scoped_number<'s>(&mut self, s: &'s HandleScope, n: f64) -> Result<Scoped<'s>, NativeError>;   // may box → must root
pub fn scoped_value<'s>(&mut self, s: &'s HandleScope, v: Value) -> Scoped<'s>;                        // root an incoming raw Value NOW

// access (resolve through the arena at call time — never stale)
pub fn scoped_get<'s>(&mut self, s: &'s HandleScope, obj: Scoped<'_>, key: &str) -> Result<Scoped<'s>, NativeError>;
pub fn scoped_set(&mut self, s: &HandleScope, obj: Scoped<'_>, key: &str, v: Scoped<'_>) -> Result<(), NativeError>;
pub fn scoped_define_data(&mut self, s: &HandleScope, obj: Scoped<'_>, key: &str, v: Scoped<'_>, flags: PropertyFlags) -> Result<(), NativeError>;
pub fn scoped_set_index(&mut self, s: &HandleScope, arr: Scoped<'_>, i: usize, v: Scoped<'_>) -> Result<(), NativeError>;
pub fn scoped_host_object<'s, T: Any>(&mut self, s: &'s HandleScope, data: T, class: &str) -> Result<Scoped<'s>, NativeError>;

// reads that don't allocate
pub fn scoped_as_str(&self, v: Scoped<'_>) -> Option<String>;
pub fn scoped_is_undefined(&self, v: Scoped<'_>) -> bool;
// ... mirror the existing Value accessors as needed by migration
```

Target call-site shape (this is the API bar — keep it this simple):

```rust
ctx.scope(|ctx, s| {
    let obj = ctx.scoped_object(s)?;
    let href = ctx.scoped_string(s, &href_text)?;
    ctx.scoped_set(s, obj, "href", href)?;
    ctx.scoped_set(s, obj, "port", ctx.scoped_number(s, port as f64)?)?;
    Ok(s.escape(ctx, obj))
})
```

Naming: methods may drop the `scoped_` prefix if a cleaner spelling falls out
during implementation (e.g. `s`-first: `alloc.string(s, ...)`); the shape —
scope token + index handles + arena-resolved access — is the requirement.

### 2.5 Interpreter-internal adoption

VM-internal helpers (`static_call_ops`, `object_internal_ops`,
`abstract_ops`, …) get the same core via
`Interpreter::with_handle_scope` + `Scoped`-based wrappers over `object::set`
/ `object::get` / `JsString::from_str`. The hand-rolled
`push_key_string_rooted` from `a999bc1f` becomes a 5-line scoped loop and the
remaining `GetOwnPropertyDescriptors` staleness (upstream arg copy) dies by
rewriting the whole branch scoped.

## 3. Phases

### P1 — core (this is the first PR)
Files: `crates/otter-vm/src/handles.rs` (new), `lib.rs` (field + module +
`with_handle_scope`), `runtime_state.rs` (trace hook),
`allocation_ops.rs` (snapshot hook).

1. `HandleArena`, `HandleScope`, `Scoped` as in §2.1–2.2.
2. Tracing wiring §2.3 (both paths).
3. Interpreter-side minimal ops: `scoped_value`, `scoped_string`,
   `scoped_object`, `scoped_get/set`, `escape`.
4. Tests (in `handles.rs` + an integration test in `crates/otter-vm/tests/`):
   - unit: push/read/truncate/nesting.
   - **move test**: allocate string A into scope, force a minor collection
     (`heap.collect_minor_with_roots` with the interpreter visitor, or the
     `test_support` heap), allocate B, assert A reads back intact (its slot
     was rewritten — compare content, and assert raw offset CHANGED to prove
     the test actually exercised a move).
   - nesting + panic-safety (scope truncates via wrapper even on `?` early
     return).
   - stress: loop of 1k scoped string+object creations with
     `gc_stress`-style forced scavenges between; verify contents.
5. Gates: `cargo test -p otter-vm` green; `cargo clippy -- -D warnings`;
   no measurable regression on `benchmarks/profile.mjs map-set` (arena trace
   adds one Vec walk per scavenge — empty arena = ~0).

### P2 — NativeCtx surface
Files: `runtime_cx.rs` (+ `js_surface.rs` where the builder lives).

1. `NativeCtx::scope` + the §2.4 method set.
2. Rewrite `ObjectBuilder` internals on top of scoped handles (public builder
   API stays; it just stops being unsound when the caller holds siblings).
3. Doc: `crates/otter-vm/src/handles.rs` module docs = the contract; add a
   "writing native functions" section to `crates/otter-macros/README.md`.
4. Gate: P1 gates + `OTTER_GC_STRESS=4` run of `-e 'new Headers(...)'`-class
   smoke lines (put them in a `tests/gc_stress_natives.rs` style harness or a
   `scripts/` shell gate).

### P3 — migrate the known-broken hot surfaces
Order: `otter-web/src/url.rs` → `otter-web/src/headers*` path →
`otter-web/src/blob.rs` → `otter-modules/src/serve.rs` (request build,
`build_server_object`).

Each migration: replace raw `Value` locals + manual roots with one
`ctx.scope`. Delete the per-site `value_roots` plumbing that becomes dead.
Gate per file: `OTTER_GC_STRESS=1,2,4,8,16` functional runs + full
`cargo test --all`; after serve.rs — k6 run, req/s must stay ≥ current
(~11.4k) since arena ops are O(1) pushes.

### P4 — reflection paths + bug #3
Rewrite `GetOwnPropertyDescriptors` / `own_property_keys_value` /
`descriptor_to_object` scoped; delete `push_key_string_rooted`; verify the
still-open truncation repro:
`OTTER_GC_STRESS=4 otter -e 'Object.getOwnPropertyDescriptors({a:1,...,g:7})'`
returns all 7 keys for strides 1..16.

### P5 — macros + enforcement
1. `#[dive]` / `raft!` / `burrow!` hand scoped handles to bodies (new arg
   shape or helper injection) so new natives are scoped by default.
2. trybuild compile-fail tests: `Scoped` escaping the closure, `Scoped` from
   scope A used with scope B (if we add a brand — optional, start with
   lifetime-only).
3. Deprecate (lint/doc) taking raw `Value` across allocating calls in native
   code; grep-based CI check for new `value_roots` additions outside the
   whitelisted core.

### P6 — cleanup + perf follow-ups (separate track, after adoption)
- Sweep now-dead `value_roots`/`slice_roots` threading (hundreds of sites).
- Root-walk cost: arena is one contiguous region; then attack the fixed
  per-scavenge root tax (`trace_roots_inner` walks IC/module/constant tables
  every scavenge — version-gate the immutable categories).
- Pre-sized object construction (bulk shape transition, one slab alloc) —
  safe to do once builders are scoped.

## 4. Non-goals (now)

- No collector changes: the generational Cheney + remembered set + incremental
  mark stack is sound and stays.
- No gc-arena/GhostCell epoch lifetimes (compile-time branding of the whole
  heap): right model, too invasive today; lifetime-scoped handles get 90% of
  the safety for 10% of the migration.
- No conservative stack scanning (blocks moving GC), no pinning natives to
  old space (kills young-gen wins on the serve hot path).

## 5. Verification matrix (every phase)

| Gate | Command |
|---|---|
| unit/integration | `cargo test -p otter-vm` (then `--all`) |
| lint | `cargo clippy --all-targets --all-features -- -D warnings` |
| stress smoke | `OTTER_GC_STRESS={1,2,4,8,16} target/release/otter -e '<repros>'` |
| stress verify | add `OTTER_GC_VERIFY=1` to the stress runs |
| conformance | `just test262-filter "Object"` delta vs baseline |
| serve perf | k6 50vus 30s vs 11.4k req/s baseline |
