# Task 56 — Remove `RefCell` from hot paths

## Goal

Eliminate every `RefCell` on the dispatch loop's per-instruction
path. Keep only the unavoidable cases (`Rc<RefCell<T>>` handles
that need shared mutability across multiple owners) and replace
the rest with plain `&mut` field access threaded through the
dispatch loop.

## Why

`RefCell` is not free:

- Every `borrow()` / `borrow_mut()` does an atomic-counter-style
  flag check, a branch, and a runtime panic path on aliasing
  violation. On a tight `LoadLocal` / `Add` / `StoreLocal` triple
  that's three borrow checks per instruction the optimiser cannot
  fully erase.
- The borrow flag is also a *correctness* hazard: any future
  parallelism (worker threads, async/Tokio bridge,
  `SharedArrayBuffer`) cannot share a `RefCell` value at all —
  it's `!Sync`. We're going to be retrofitting the whole graph
  the moment we ship task 35.
- We already learned the lesson on the microtask queue (task 33):
  the right shape was `&mut MicrotaskQueue` threaded through
  `dispatch_loop`, not `RefCell<MicrotaskQueue>`. The same
  pattern applies to the value model.

## Scope

1. **Inventory.** List every `RefCell` in `crates-next/*` with
   line numbers. Annotate each with hot-path / cold-path /
   handle-shared.
2. **Hot path** (run on every instruction, e.g. `JsObject::set`,
   `JsArray::get`, `IteratorState::advance`, `Shape::transitions`):
   replace with one of:
   - Plain `&mut` field access on the owning frame / interpreter,
     threaded through `dispatch_loop` alongside `&mut stack`.
   - `Cell<T>` for `Copy` slots (e.g. `last_index: Cell<u32>` on
     `JsRegExp` already — keep that pattern).
   - Owned `Box<T>` plus `&mut` access where the value has a
     single conceptual owner.
3. **Handle-shared path** (`Rc<RefCell<ObjectBody>>` etc.): these
   are genuine shared-mutability cases. Three alternatives to
   evaluate:
   - **`UnsafeCell` behind a small invariant module** — best
     perf, requires `unsafe`, blocked by workspace-wide
     `unsafe_code = "forbid"`. Would need an ADR to relax.
   - **`Cell<HandleSlot>`** with a copyable slot type — works for
     small slot enums.
   - **Hidden-class lock-free swap** — store `Rc<Body>` and use
     `std::mem::swap` under `&mut` to mutate; readers always see
     a consistent snapshot. This is what V8 / JSC do for shape
     transitions.
4. **Benchmark each replacement.** Foundation has no benches yet
   (task 50 is open). Add one criterion bench per touched file
   so the perf delta is recorded and regressions are caught.

## Out of scope

- Removing `RefCell` from `crates/*` legacy stack.
- Adopting `unsafe`. If `UnsafeCell` is the right answer for any
  individual hot field, write the ADR amendment as a separate
  follow-up; do not silently introduce `unsafe` blocks here.

## Files / directories you may touch

- `crates-next/otter-vm/src/object.rs` (likely largest delta).
- `crates-next/otter-vm/src/array.rs`.
- `crates-next/otter-vm/src/regexp.rs` (already `Cell`-based for
  `lastIndex`, but check `JsRegExpBody`).
- `crates-next/otter-vm/src/lib.rs` (`Frame` / `IteratorState`).
- `crates-next/otter-vm/benches/` (new bench targets).

## Acceptance criteria

- `rg "RefCell" crates-next/otter-vm/src/` shows only
  handle-shared cases, each annotated with a `// hot? cold?`
  comment.
- A criterion bench compares before / after on each touched
  type's hottest API. Report the delta in the closing summary.
- Engine fixture suite still 100% green.
- No `unsafe` introduced without a separate ADR.

## Coordination

- **Task 33** (microtask queue) already follows this rule and is
  the reference shape — `&mut MicrotaskQueue` threaded into
  `dispatch_loop` alongside `&mut stack`.
- **Task 55** (`otter-macros-next`) — coordinate so the new
  proc-macro doesn't regenerate `RefCell` patterns we're trying
  to erase.
- **Task 35** (async/await) is blocked by this when the worker
  thread story lands; sequence accordingly.

## Risks

- API ripple. Many `&self` methods will flip to `&mut self`.
  Public API consumers (`Otter::run_*`, `Runtime::run_*`) need to
  flip too — already started for task 33.
- Borrow-checker friction. Some legitimate aliasing patterns
  (object self-referencing through prototype chain) will need
  refactor work. Plan small slices, not one big PR.

## Status

- not started
