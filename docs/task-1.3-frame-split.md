# Task 1.3 — Hot/Cold Frame Split (Design)

Status: design fixed 2026-05-23. Implementation on branch
`task-1.3-frame-split`.

Owning plan entry: `docs/architecture-refactor-plan-2026-05.md` →
Task 1.3. Phase 1 (Tasks 1.1, 1.2, 1.4) shipped; `Value` is 8B `Copy`
NaN-boxed, hot payloads in GC bodies, `ExtraRoots` callback wired.

## Current state

`Frame` in [crates/otter-vm/src/frame_state.rs](../crates/otter-vm/src/frame_state.rs)
is **488 bytes**. Lives in a dispatcher-local
`SmallVec<[Frame; 8]>`; every call site that touches the running frame
takes `&mut SmallVec<[Frame; 8]>` plus `&mut Interpreter` as split
borrows.

Field-by-field size (measured via probe test, removed after design):

| Field | Bytes | Hot? | Notes |
|---|---:|---|---|
| `function_id: u32` | 4 | yes | every dispatch tick |
| `pc: u32` | 4 | yes | every dispatch tick |
| `registers: SmallVec<[Value; 8]>` | 80 | yes | every load/store/arith |
| `return_register: Option<u16>` | 4 | warm | only on return |
| `upvalues: Rc<[UpvalueCell]>` | 16 | warm | LoadUpvalue/StoreUpvalue |
| `this_value: Value` | 8 | warm | GetThis, method dispatch |
| `handlers: SmallVec<[TryHandler; 4]>` | 96 | cold | EnterTry / unwind only |
| `pending_throw: Option<Value>` | 16 | cold | EndFinally only |
| `construct_target: Option<JsObject>` | 8 | cold | constructor return |
| `rest_args: SmallVec<[Value; 4]>` | 48 | cold | CollectRest |
| `new_target: Option<Value>` | 16 | cold | GetNewTarget |
| `incoming_args: SmallVec<[Value; 4]>` | 48 | cold | CollectArguments |
| `async_state: Option<AsyncFrameState>` | 8 | cold | Await / async return |
| `module_url: Rc<str>` | 16 | cold | ImportNamespace |
| `pending_to_primitive: Option<PendingToPrimitive>` | 20 | cold | rare protocol step |
| `pending_bind_function: Option<PendingBindFunction>` | 80 | cold | Function.prototype.bind |
| `pending_get_iterator: Option<PendingGetIterator>` | 12 | cold | GetIterator user-object path |
| `pending_iterator_next: Option<PendingIteratorNext>` | 28 | cold | IteratorNext user-object path |
| `generator_owner: Option<JsGenerator>` | 8 | cold | Yield only |

Subtotal **cold = ~404 B** out of 488. ~83% of the cache footprint is
state most function calls never touch.

GC tracing currently walks all of them every minor cycle
(`trace_frame_slots`).

## Target layout

### Hot frame (`Frame`)

```rust
#[repr(C)]
pub struct Frame {
    pub function_id: u32,            // 4
    pub pc: u32,                     // 4
    pub this_value: Value,           // 8
    pub registers: SmallVec<[Value; 8]>, // 80
    pub upvalues: Rc<[UpvalueCell]>, // 16
    pub return_register: Option<u16>,// 4
    pub cold: Option<ColdFrameIdx>,  // 4 (NonZeroU32 niche)
}
```

Total: **120 B** (target ≤ 128 B). Cache lines per frame drop from 8 to
2, GC scan walks 2 explicit slot groups instead of 14, hot read-set fits
in one cache line for the dispatcher's per-tick decoding (function_id,
pc, this_value, plus the register window pointer/header).

> Note on PC width: the plan calls for byte-offset PC. The current
> bytecode (`ExecInstr` index) does not yet have a byte stream; that
> migration is Phase 2.1. `pc: u32` keeps both semantics; switching to
> byte offsets is a name/semantics change, not a layout change, and is
> deferred. Acknowledged in plan.

### `ColdFrameIdx`

`Option<ColdFrameIdx>` is niche-optimized: `ColdFrameIdx(NonZeroU32)`.
`None` means **no cold record allocated**. Frames stay cold-less until
the first opcode that needs cold state runs.

### Cold record (`ColdFrame`)

```rust
pub struct ColdFrame {
    pub handlers: SmallVec<[TryHandler; 4]>,
    pub pending_throw: Option<Value>,
    pub construct_target: Option<JsObject>,
    pub rest_args: SmallVec<[Value; 4]>,
    pub new_target: Option<Value>,
    pub incoming_args: SmallVec<[Value; 4]>,
    pub async_state: Option<AsyncFrameState>,
    pub module_url: Rc<str>,
    pub pending_to_primitive: Option<PendingToPrimitive>,
    pub pending_bind_function: Option<Box<PendingBindFunction>>,
    pub pending_get_iterator: Option<PendingGetIterator>,
    pub pending_iterator_next: Option<PendingIteratorNext>,
    pub generator_owner: Option<crate::generator::JsGenerator>,
}
```

`PendingBindFunction` is `Box`'d inside the cold record (80 B payload
hurts cold-record growth when the freelist holds many slots). Other
pending records stay inline.

`module_url`: moved to `ColdFrame` because reads come from
`ImportNamespace` only. Frames never touched by import incur zero
`Rc<str>` clone cost.

### Cold storage — pool

`Interpreter` owns:

```rust
struct ColdFramePool {
    slots: Vec<ColdFrame>,         // index = ColdFrameIdx as usize - 1
    free: Vec<u32>,                // freelist (0-based slot indices)
}
```

- Acquire: pop from `free`; if empty, push fresh `ColdFrame::default()`
  and return its index.
- Release: reset slot to default (`std::mem::take` returns owned cold
  value to caller before slot reset; not needed for normal pop), push
  index onto `free`.
- No `Box<ColdFrame>` per frame. Backing storage is one `Vec<ColdFrame>`
  amortized; growth is monotonic but bounded by peak concurrent cold
  state (typically dozens, not thousands).

Indexing uses `u32` (4 B in hot frame). `Option<ColdFrameIdx>` =
`Option<NonZeroU32>` = 4 B (niche-encoded). Pool slots store at
`slots[idx.get() - 1]`.

### Async / generator parking

When a frame is parked (off the dispatcher's `stack`) for an
`await`/`yield`, its cold record must travel with it — pool indices are
not stable across parking because subsequent frames may reuse the slot.

Detach protocol on park:

```rust
fn detach_cold(frame: &mut Frame, pool: &mut ColdFramePool)
    -> Option<ColdFrame>
{
    let idx = frame.cold.take()?;
    let owned = std::mem::take(&mut pool.slots[idx.get() as usize - 1]);
    pool.free.push(idx.get() - 1);
    Some(owned)
}
```

Re-attach on resume:

```rust
fn attach_cold(frame: &mut Frame, pool: &mut ColdFramePool,
               cold: ColdFrame) {
    let idx = pool.acquire();
    pool.slots[idx.get() as usize - 1] = cold;
    frame.cold = Some(idx);
}
```

Parked storage:
- `AsyncFrameState`'s resume continuation already saves a frame
  snapshot. That snapshot now also stores `Option<Box<ColdFrame>>`.
- `JsGenerator` stores the suspended frame on its body. Same change.

Boxing only at the parking boundary keeps the pool from being holed by
long-suspended generators while a busy dispatcher reuses slots.

### GC root tracing

`Frame::trace_frame_slots` shrinks to:

```rust
fn trace_frame_slots(&self, pool: &ColdFramePool,
                     visitor: &mut SlotVisitor<'_>)
{
    for v in &self.registers { v.trace_value_slots(visitor); }
    for slot in self.upvalues.iter() {
        visitor(slot as *const _ as *mut RawGc);
    }
    self.this_value.trace_value_slots(visitor);
    if let Some(idx) = self.cold {
        pool.slots[idx.get() as usize - 1].trace_cold_slots(visitor);
    }
}
```

Cold tracer walks rest/incoming args, new_target, construct_target,
async_state, pending_*, generator_owner. Existing test
`gc_invariants::root_enumeration` continues to assert the union is
visited.

Async/generator-parked cold records are traced through their owner
(promise registry / generator body) which already roots them today.

`RuntimeState::trace_roots` gains the `&ColdFramePool` borrow to forward
to each frame's tracer. `ExtraRoots` trampoline contract from Task 1.4
is unchanged in shape — the trampoline still calls
`RuntimeState::new(interp).trace_roots(visitor)`; the pool just becomes
part of what `RuntimeState` reads through `&Interpreter`.

### Frame-size assertion

`crates/otter-vm/src/frame_state.rs` gains:

```rust
const _: () = assert!(
    std::mem::size_of::<Frame>() <= 128,
    "hot Frame must stay within two cache lines",
);
```

Cold record has no static cap (it's pool-allocated, allocation cost
amortized) but we add a probe test that prints the size for
regression awareness.

### Access pattern

Cold-touching ops gain helpers on `Interpreter`:

```rust
impl Interpreter {
    fn frame_cold(&self, frame: &Frame) -> Option<&ColdFrame>;
    fn frame_cold_mut(&mut self, frame: &mut Frame) -> Option<&mut ColdFrame>;
    fn frame_ensure_cold(&mut self, frame: &mut Frame) -> &mut ColdFrame;
}
```

Call shape:

```rust
// Before
frame.handlers.push(handler);

// After
self.frame_ensure_cold(frame).handlers.push(handler);
```

Sites that only read (no allocation if absent):

```rust
// Before
if let Some(t) = &frame.construct_target { ... }

// After
if let Some(t) = self.frame_cold(frame).and_then(|c| c.construct_target.as_ref()) { ... }
```

Borrow safety: every cold-touching site already has `&mut self`
(Interpreter) plus `&mut SmallVec<[Frame; 8]>` (stack). The cold pool
lives on `Interpreter`. Helpers take `&mut self` and a `&mut Frame` —
that's two independent borrow paths (interpreter field vs. stack slot).
The borrow checker accepts them as split borrows because the pool
helper only touches `self.cold_frame_pool`, never the rest of self.
Where the checker rejects the split, a small `take_cold_idx →
work_on_pool → restore_cold_idx` swap pattern works around it without
copies.

## Migration order

Every step compiles, passes `cargo test -p otter-vm --lib` and
`cargo test -p otter-runtime --lib`, and `cargo clippy --all-targets
--all-features -- -D warnings`.

1. **Scaffolding.** Introduce `ColdFrame`, `ColdFrameIdx`,
   `ColdFramePool`. Wire pool into `Interpreter` (field + clear-on-
   drop). Add `Interpreter::frame_cold{,_mut,ensure}` helpers. No field
   migration yet. Frame still 488 B. Green.
2. **Move `pending_*` quartet** (`pending_to_primitive`,
   `pending_bind_function`, `pending_get_iterator`,
   `pending_iterator_next`). These are the rarest. Each removes one
   field from `Frame`, adds to `ColdFrame`, rewrites the ~10 call sites.
   Green after each.
3. **Move return-time cold** (`pending_throw`, `construct_target`,
   `new_target`). Touched in `pop_frame` / construct return / throw
   unwind.
4. **Move entry-time cold** (`rest_args`, `incoming_args`). Touched
   only at function entry by `CollectRest` / `CollectArguments`.
5. **Move `handlers`.** EnterTry/LeaveTry/throw unwind. Touch sites
   include async unwind paths.
6. **Move `module_url`.** ImportNamespace only.
7. **Move `async_state` and `generator_owner`.** Async/generator
   parking detach/attach lands here with the cold-detach helpers
   described above.
8. **Tracing.** Update `trace_frame_slots` signature to take
   `&ColdFramePool`; thread through `RuntimeState::trace_roots` and
   `gc_invariants` tests. The `ExtraRoots` trampoline already routes
   through `RuntimeState::new(interp).trace_roots`, so no
   trampoline-shape change.
9. **Assertion.** `const _: () = assert!(size_of::<Frame>() <= 128);`
   plus a `#[test]` probe that prints both sizes for regression
   visibility.
10. **Doc + plan update.** Mark Task 1.3 DONE.

Each step is one commit. Each leaves the tree green.

## Acceptance signals

- `cargo test -p otter-vm --lib` 532 / 532.
- `cargo test -p otter-runtime --lib` 123 / 123.
- `cargo test --all --all-features` no regressions.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `bash scripts/test262-safe.sh` over `built-ins/Function`,
  `built-ins/Promise`, `built-ins/Iterator`, `language/expressions/await`,
  `language/statements/generators`, `language/statements/try`,
  `language/module-code` — zero regression vs Phase 1 baseline.
- `static_assertions` / `const _` proof that `size_of::<Frame>() <= 128`.
- `gc_invariants::root_enumeration` continues to enumerate every frame
  field through the pool.

## Out of scope

- Byte-offset PC (Phase 2.1).
- Bytecode v2 (Phase 2.1).
- JIT stack maps (Phase 2 / future).
- Replacing `SmallVec<[Value; 8]>` registers with a shared register
  buffer (Sparkplug-style). The plan's "register window" wording could
  be read as advocating that; defer until Phase 2.x has frozen the
  call/return ABI and we have bench data showing the inline buffer is
  the bottleneck. The current 80 B inline path keeps the cache hot for
  small functions today.

## Risks

- **Async/generator detach correctness.** Lose a cold record on park →
  silent dropped handlers / pending throws on resume. Mitigation:
  detach helper returns `Option<ColdFrame>` not `Option<Box<ColdFrame>>`,
  callers always wrap into the saved frame; gc_invariants suite
  validates round-trip.
- **Borrow-checker friction.** Most call sites already split-borrow
  cleanly. Where they don't, swap-out helpers avoid `unsafe`.
- **GC tracing miss.** Adding fields to `ColdFrame` without updating
  `trace_cold_slots` is the same risk as today's `trace_frame_slots`.
  Existing `gc_invariants::root_enumeration` test catches it.
