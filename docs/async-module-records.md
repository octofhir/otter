# Async Module Records — design

Goal: spec-shaped Cyclic Module Record evaluation (§16.2.1.5) so
top-level-await modules park without blocking sibling evaluation, and
every evaluation consumer (static graph, dynamic `import()`, deferred
namespaces) settles through per-module promises instead of ad-hoc
sequencing.

Driving test: `language/module-code/top-level-await/`
`async-module-does-not-block-sibling-modules.js` — the last failing
test in that directory (250/251).

```
main:  import "./tla_FIXTURE.js";          // async: sets check=false, await 0, sets check=true
       import { check } from "./sync_FIXTURE.js";  // export const { check } = globalThis;
       assert.sameValue(check, false);     // sync sibling ran while tla was parked
```

Required order: tla starts → parks at `await 0` → **sync runs
immediately** (reads `check === false`) → microtask resumes tla →
main runs after all deps settle → `check` binding (from sync) is
`false`.

## Current state (what blocks this)

- `module_graph::build_entry_body` synthesises a bytecode `<entry>`
  that calls each `<module-init>` in topological order and — when the
  graph contains TLA — awaits each async init **inline, in
  sequence**. A parked init therefore blocks every later sibling.
- `Interpreter::evaluate_module_rec` (static/deferred path) is a
  synchronous DFS; an async target is a hard `TypeError` there.
- `evaluate_module_rec_dynamic` + `module_async_init_promises`
  (commit `af13fa9b`) already run an async init through
  `run_module_init` (async result promise, §16.2.1.9 wiring mirrored
  from `run_inner`) and gate the import promise on it — but only for
  the dynamic-import path, with a flat per-module promise cache and
  no parent/dependency bookkeeping.
- Separate small bug: exports metadata does not collect BoundNames of
  destructuring declarations (`export const { check } = …`), so the
  sibling fixture fails resolution before ordering even matters.

## Design

### 1. Export destructuring BoundNames (prerequisite, small)

Compiler exports collection (wherever `CompiledExport` records are
built from `export const/let/var <decl>`) must walk binding patterns
(object/array, nested, rest, defaults) and emit one export record per
bound name, plus the matching module-env slot stores in lowering.
Spec: BoundNames of LexicalBinding / VariableDeclaration. oxc gives
`BindingPattern`; reuse the compiler's existing pattern-walk helper if
one exists (hoisting already needs BoundNames — find it, do not write
a second walker).

### 2. `ModuleRecordState` (otter-vm, interpreter-owned)

One struct per linked module URL, owned by the `Interpreter` (same
lifecycle as `module_environments`, cleared together):

```rust
struct ModuleRecordState {
    status: ModuleStatus,        // New | Evaluating | EvaluatingAsync | Evaluated
    has_tla: bool,               // <module-init> is_async
    // [[AsyncEvaluation]] is true iff async_order.is_some();
    // the u64 preserves the spec's true-ordering for
    // AsyncModuleExecutionFulfilled's sorted gather.
    async_order: Option<u64>,
    pending_async_dependencies: usize,
    async_parent_modules: Vec<Arc<str>>,
    // [[TopLevelCapability]]-shaped gate. Some for every module that
    // evaluates async (not only roots — flat approximation of the
    // spec's capability-on-cycle-root; our SCC handling collapses to
    // the DFS cycle check already in evaluate_module_rec).
    evaluation_promise: Option<JsPromiseHandle>,
    evaluation_error: Option<Value>,   // rethrow cache (replaces module_errors entry)
}
```

GC: `evaluation_promise` + `evaluation_error` traced from
`RuntimeState::trace_roots` (extend the existing module_errors walk).
Supersedes and absorbs `module_evaluating` / `evaluated_modules` /
`module_errors` / `module_async_init_promises` — four ad-hoc maps
become one record map. Keep the old accessors as thin views during
migration, delete at the end.

### 3. InnerModuleEvaluation (§16.2.1.5) in Rust

Replace the body of `evaluate_module_rec` with the spec algorithm
over records (single implementation; the `_dynamic` variant and the
deferred-namespace path call the same function and differ only in how
they consume the result):

```
evaluate_module(url) -> Result<Option<JsPromiseHandle>, VmError>
  // None        => evaluated synchronously, done
  // Some(p)     => evaluating async; p settles when subtree settles
```

- status Evaluated → return cached (error → rethrow, promise → Some).
- status Evaluating/EvaluatingAsync (cycle) → return current
  state's promise (None if sync-evaluating).
- mark Evaluating; recurse into eager deps **in source order**;
  for each async dep: `pending_async_dependencies += 1`, register
  self in dep's `async_parent_modules`. **Do not await deps** — that
  is the whole point: a parked dep contributes a pending count, the
  loop continues to the next sibling.
- after deps: if `pending_async_dependencies > 0 || has_tla`:
  - allocate `evaluation_promise` (pending, runtime-rooted),
    set `async_order = next_counter()`, status EvaluatingAsync;
  - if `pending_async_dependencies == 0` → ExecuteAsyncModule now:
    `run_module_init` (existing), attach
    AsyncModuleExecutionFulfilled/Rejected reactions to the init's
    result promise (native fns; url in the Arc capture, promise
    handles in `captures` so the GC traces them — **no Rc, no
    side-channel counters**: all state lives in the records map).
  - else the init runs later, triggered by the last dep's
    fulfilled-walk.
  - return Some(evaluation_promise).
- else run init synchronously (current behaviour), status Evaluated,
  return None. Sync throw → evaluation_error + propagate (existing
  module_errors semantics).

### 4. AsyncModuleExecutionFulfilled / Rejected (§16.2.1.9)

Plain `&mut Interpreter` methods called from the init-promise
reactions:

- Fulfilled(url): status Evaluated; settle `evaluation_promise`
  fulfilled; for each `async_parent_modules` entry: decrement
  `pending_async_dependencies`; if it hits 0 and the parent is
  EvaluatingAsync → ExecuteAsyncModule(parent) (which re-enters this
  walk on its completion). Spec gathers available ancestors sorted by
  [[AsyncEvaluation]] order — use `async_order` for the sort.
- Rejected(url, reason): status Evaluated with
  `evaluation_error = reason`; reject `evaluation_promise`; propagate
  the rejection up `async_parent_modules` recursively (spec walk).

### 5. Entry driver simplification

`build_entry_body` currently emits the init-call chain (plus
register/env plumbing comments in module_graph.rs §"Synthesise"). New
shape: `<entry>` calls **one** opcode, `Op::EvaluateModule
k[entry_url]` (already exists for the sync path), whose handler is
the new `evaluate_module(url)`. If it returns Some(promise) the entry
awaits it (entry already compiles async when the graph has TLA — keep
that bit; it awaits ONE promise now instead of N inits).

This deletes: per-module init-call emission, the deferred-async
eager-evaluation special case (`deferred_async_modules` — records make
it unnecessary; `import defer` of an async target evaluates eagerly by
just calling `evaluate_module` from the entry as the proposal
requires), and the TLA-descendants fixpoint in `build_entry_body`.

Risk concentrates here: every module-code test exercises the entry
driver. Land 2–4 first behind the existing driver (records updated in
parallel, asserts equal behaviour), then swap the driver in one
commit with full module-code/statements/Promise guards.

### 6. Dynamic import + deferred namespaces converge

- `Op::ImportNamespaceDynamic`: call `evaluate_module(target)`;
  None → fulfilled(namespace) (today's fast path);
  Some(p) → gate via existing `settle_promise_on_async_init`
  (rename: `settle_promise_on_module_evaluation`).
  Delete `evaluate_module_rec_dynamic` + `module_async_init_promises`
  (absorbed by records).
- Deferred namespace force-eval keeps its `TypeError` for
  EvaluatingAsync-not-settled targets (§28.3 ReadyForSyncExecution
  checks the record status instead of `function.is_async`).
- Runtime host-loader path (`begin_dynamic_import` /
  `PendingAsyncEvaluation`): replace the manual init loop in
  `load_dynamic_module` with `evaluate_module` on the linked context;
  gate the registry token on the returned promise
  (`settle_dynamic_import_on_async_inits` keeps its shape, takes one
  promise).

## Order of work

1. Export destructuring BoundNames (+ guard:
   `import {check} from sync_FIXTURE` resolves; module-code diff).
2. Records struct + trace + migration of the four maps (behaviour
   identical; full guards).
3. evaluate_module + ExecuteAsyncModule + Fulfilled/Rejected walks;
   dynamic-import path switched; old `_dynamic` deleted (guards:
   TLA dir, dynamic-import, rejection-order tests).
4. Entry-driver swap (guards: full module-code + statements +
   Promise + 5× stress).
5. Cleanup: dead code, docs (`//!` on touched modules), conformance
   note, memory update.

## Invariants to keep

- All reaction state in interpreter-owned records — never in Rust
  closures (no `Rc`); promise handles in native-fn `captures`.
- `run_module_init` stays the single async-init entry (§16.2.1.9
  wiring); do not duplicate its promise/async_state setup.
- Evaluation order of sync graphs must not change (source-ordered
  eager deps — `module_resolutions` are already source-ordered, see
  module_graph.rs request_order comment).
- Every step: `cargo test -p otter-vm --lib` (570),
  `-p otter-runtime --lib` (138), fail-list diff vs
  `test262_results/latest.json`, zero regressions.

## Acceptance

- `language/module-code/top-level-await/` 251/251.
- `language/module-code/` ≥ 586 pass, 0 regressions elsewhere
  (statements 8695, Promise 676, dynamic-import 684 minimums).
- Known out of scope: host-loader HTTPS dynamic imports keep the
  current approximation; `for_of_iterator_close.rs::
  close_on_return_runs_return` is a pre-existing unrelated failure.

## Implementation session (2026-06-05)

All five steps landed; acceptance met.

1. **Export destructuring BoundNames** — pre-pass
   (`entry.rs`), metadata (`compiled_module.rs`), and
   `destructure_pattern` leaf mirror now walk binding patterns via
   `collect_pattern_var_names`. `export const { check } = …` resolves.
2. **`ModuleRecordState`** (`otter-vm/src/module_records.rs`) absorbed
   `module_evaluating` / `evaluated_modules` / `module_errors` /
   `module_async_init_promises`; gates + errors traced from
   `RuntimeState::trace_roots`.
3. **Evaluate / InnerModuleEvaluation** (`module_ops.rs`) — full spec
   shape, including the DFS stack, `[[DFSIndex]]` /
   `[[DFSAncestorIndex]]`, SCC pop with `[[CycleRoot]]`, per-module
   gate promises, `[[PendingAsyncDependencies]]`, and §16.2.1.9
   ExecuteAsyncModule + AsyncModuleExecutionFulfilled/Rejected with a
   faithful GatherAvailableAncestors (gather-then-sort-then-execute —
   nested-on-discovery execution reorders `dfs-invariant.js`).
   Waiters on a cycle member register on its cycle root
   (`pending-async-dep-from-cycle.js`). Dynamic import +
   host loader (`load_dynamic_module`) call `evaluate_module`;
   `evaluate_module_rec_dynamic` and the init-marker dedupe died.
4. **Entry driver** — `<entry>` emits `Op::EvaluateModule dst, k[url]`
   (operand layout changed to `[reg, const]`; gate promise or
   `undefined` lands in `dst`) for the entry plus idempotent sweeps,
   awaiting gates when the graph is async. Import-defer TLA roots are
   no longer pre-evaluated by the driver: InnerModuleEvaluation
   gathers them in request position
   (GatherAsynchronousTransitiveDependencies), fixing the
   `import-defer/evaluation-top-level-await` family (4 tests) that the
   pre-entry approximation evaluated before earlier siblings.
5. Docs + cleanup (this note).

Results: `language/module-code` 584 → 587/599;
`top-level-await` filter (incl. `import-defer`) 256/256, stable 5×;
dynamic-import 689, statements 8714 (2 pre-existing crashes),
Promise 676/676. Zero regressions; `otter-vm` 570 / `otter-runtime`
138 lib tests green. `tokio_spawn_native_ctx_is_not_send` trybuild
mismatch pre-exists on main.
