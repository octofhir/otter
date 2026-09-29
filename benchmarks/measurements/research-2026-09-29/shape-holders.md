# Shape-reference holders outside `ObjectBody.shape` (at 932e65fa)

Input for L2b-0 (collectable shapes; architecture report section 12).
Shape ids are never reused (monotonic counter, 0/1 reserved), so an
id-keyed or id-compared stale entry only misses. Handle (u32 offset)
comparisons are ABA-exposed once dead shape cells are swept and their
ranges reused, and the shaped fast paths skip bounds checks on a match.

## Rust-side holders
| # | Holder | Value | Traced today | If a shape dies |
|---|---|---|---|---|
| 1 | `ShapeRuntime.root` | handle | root | keep strong |
| 2 | `ShapeRuntime.handles_by_id` | handle | root | prune in the weak pass (compile bakes resolve through it) |
| 3 | `ShapeRuntime.transitions` | child handle | root | prune (dead child would be installed into objects) |
| 4 | `ShapeRuntime.offset_cache` | ids | no | prune with #2 |
| 5 | `simple_constructor_shape_cache` (fid) | handle | root | keep strong (per function, purged on chunk eviction) |
| 6 | `arguments_shape_cache` | handle | root | keep strong (bounded) |
| 7 | `object_literal_layouts` | `ObjectLayout{ShapeId}` | no | dead id = hard `TypeMismatch` ⇒ hold the handle strongly |
| 8 | `object_layout_cache` | `ObjectLayout` | no | same as #7 |
| 9 | embedder `ObjectLayout` | ShapeId | no | kept alive through #8 |
| 10 | `constructor_field_transition_cache` | ids | no | miss |
| 11 | `constructor_prototype_shape_cache` | ids | no | miss |
| 12 | `PropertyLookupCache` (hops≥1 holder handle) | handle | no | ABA ⇒ prune |
| 13 | `StoreTransitionCache.ways` | ids + `to_shape` | `to_shape` strong | keep strong (bounded table); `jit_ways.target_shape` paired |
| 14 | `CacheStub.shape_ids` (CodeBlock feedback) | ids | no | miss |
| 15 | `CacheStub.hits` | handle | no | ABA ⇒ prune |
| 16 | `CacheStub.transitions` | `to_shape` handle | strong via code space | keep strong while the stub lives |
| 17 | `method_ics` Ordinary hit | handle | no | ABA ⇒ prune |
| 18/19 | method ICs / `method_targets` ids | ids | no | miss |
| 20 | `MonoNativeLeaf{recv,holder}_shape_offset` | raw u32 | no | ABA ⇒ prune |
| 21 | `global_object_load_ic` | id | no | miss |
| 22 | JSON `key_cache` | id | per call | miss |
| 24 | `ClosureRareBody.prototype_shape` | raw u32 in a GC cell | no | ABA ⇒ make it a traced handle |

## Generated code
Shapes live only as machine-code immediates: CacheIR `GuardShape` /
`PublishShape` (publish = use-after-free if the shape dies), receiver
allocation plans (publish), constructor field transitions (publish),
method guards, inline property shapes, guarded method calls, the global
object shape. No code object records the shapes it embeds; dependency lists
are empty in both tiers. Invalidated code runs until retirement, so a
published shape must outlive retirement: code objects must hold their
shapes strongly until then.

## GC hooks
`PostMarkProcessor = fn(&mut GcHeap)` has no interpreter context. The
`ExtraRootSource::prepare_collection(&self, &GcHeap)` hook runs with valid
mark bits at the sweep boundary but also before scavenges and incremental
steps; a dedicated post-mark weak hook on the source is the clean seam.
Pruning must follow the ephemeron fixpoint.

## Transient hazards
Shapes held in Rust locals across allocations (layout install, simple
constructor receivers, arguments shapes, compile-time bakes before later
allocating bakes, `child_with_roots` results) are safe only while shapes
are immortal; with collectable shapes each needs a root.

## Snapshot
Restore registers every image shape into `handles_by_id` (rooted forever);
with weak tables the first full collection prunes unreachable ones. The
code space (with handle-bearing feedback) is shared between donor and
restored isolates — an existing hazard to verify.
