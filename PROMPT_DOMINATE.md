# PROMPT_DOMINATE — rebuild the engine until it beats node and bun everywhere

Goal: otter beats node AND bun on retired instructions and wall-clock, with
full correctness, on real workloads. The Web Tooling Benchmark (WTB, 18 real
parsers, compilers, minifiers and formatters) is the thermometer. The Octane
fixed-work matrix must not regress. Benchmarks measure the engine; never fit
code to them.

Work autonomously. Do not ask questions, stop at checkpoints or hand back. On
a session limit, schedule a wakeup and continue from the memory notes. Follow
the ub-mode skill: failing probe first, root causes, an action log reported
at the end.

## The one rule: refactor toward the reference engines, never patch

otter today does 2.16x node's instructions on WTB. That gap is architecture,
not tuning. Every slice:

1. **Decompose first.** Measure where otter's instructions go for the
   subsystem (profile shares, `op_instr.sh` per-operation counts against
   node). Write the numbers down in the memory note.
2. **Read the reference end to end** before writing code: the V8, JSC and
   SpiderMonkey files listed for that slice below, in `~/work/engines`
   (sparse checkouts; add a directory with `git sparse-checkout add` when a
   file is missing). Note how they lay out the data, where the fast path
   lives (generated code vs C++), and what the slow path does.
3. **Port the architecture**, adapted to our object model, in one slice.
   Delete what it replaces in the same slice: no flags, toggles, bridges,
   second copies, versioned formats, fallback "just in case" paths.
4. **Size check.** A slice is worth landing when it moves the WTB
   instruction geomean by at least 3%, or a listed per-operation target by
   at least 2x. If it does not, the scope is too small: widen it to the whole
   subsystem instead of adding more small fixes. Chains of 1–3% tweaks,
   per-probe special cases, instrument-and-patch loops and bench-fitting are
   banned.
5. Never name V8, JSC, SpiderMonkey, Maglev, TurboFan, etc. in code,
   comments, identifiers or commit messages. They may be named here and in
   memory notes.
6. Every failure is ours. Never write "pre-existing" or "test rot". A slice
   that regresses anything fixes it forward before it is committed.

## Measuring

- **Instructions are the primary metric** (WTB runs 20 fixed samples per
  target, so a whole run is fixed work, and other sessions on this machine
  make wall time swing 2–3x):
  `scripts/dev/web_tooling_instr.sh <before-bin> <after-bin> <out>`.
  Whole-run counts include bundle parse/compile; compare like with like.
  Repeat a target 3 times and take the median before calling a ±2% move.
- Wall clock, when the machine is quiet (check `uptime`, load < 3):
  `scripts/dev/web_tooling_matrix.sh <bin> <out>` (runs/s, otter and node).
- Per-operation instructions against node and bun:
  `scripts/dev/op_instr.sh <bin> [N] [case...]` (cases include the native,
  callback, property, element and call families). Probes must run inside
  functions; top-level loops take other paths.
- Profiles: `scripts/dev/web_tooling_profile.sh <bin> <dir>`, then
  `scripts/dev/web_tooling_hot.py <dir> self|exits N`; drill down with
  `jit_exits.py <sample> N <entry> <frames>`, `children_of.py`,
  `caller_tier.py` (needs `--perf-basic-prof`), `isolate_self.py`,
  `sample_isolate.sh` (retries when `sample` attaches early).
- Octane: `scripts/dev/wallclock_matrix.sh target/release/otter benchmarks/results/<slice>/m 5`.

## Standings (2026-10-30, HEAD 1038d2bc6)

WTB retired instructions, whole run, otter / node (`benchmarks/results/nabi/instr_vs_node.txt`):

| target | otter | node | x |
|---|---|---|---|
| coffeescript | 129.2G | 42.5G | 3.04 |
| babel | 99.3G | 33.9G | 2.93 |
| lebab | 101.2G | 35.3G | 2.87 |
| postcss | 127.3G | 44.3G | 2.87 |
| babel-minify | 87.7G | 32.3G | 2.71 |
| uglify-js | 101.3G | 37.8G | 2.68 |
| chai | 76.9G | 29.0G | 2.65 |
| acorn | 80.5G | 30.7G | 2.62 |
| buble | 98.5G | 38.1G | 2.58 |
| jshint | 70.4G | 30.3G | 2.33 |
| prettier | 95.9G | 46.0G | 2.09 |
| esprima | 78.1G | 42.5G | 1.84 |
| prepack | 89.7G | 49.0G | 1.83 |
| source-map | 79.6G | 45.0G | 1.77 |
| espree | 65.6G | 39.9G | 1.64 |
| typescript | 59.3G | 39.2G | 1.51 |
| terser | 41.8G | 28.7G | 1.46 |
| babylon | 44.3G | 41.5G | 1.07 |

Geomean 2.16x. Wall runs/s was 3.88x node on a quiet machine (2026-10-30
morning). Octane matrix 1.43x node, 1.80x bun.

### Where otter's time goes (WTB mean of 18 profiles, first Rust frame below generated code)

| share | where | what it is |
|---|---|---|
| 37% | generated code | compiled JS itself |
| 17% | `native_exit_entry` | bodies of natives called from JS, including the Rust loop of callback methods |
| 6.0% | `jit_load_property_stub` | 5.1% in `jit_runtime_load_property_value`: named-load IC misses |
| 4.9% | `jit_load_element_stub` | element loads: `load_element_values`, `resolve_property_load`, generic `[[Get]]`, keyed atomization, feedback recording |
| 4.3% | `otter_native_invoke` | Host-kernel calls: `call`/`apply`/`bind`, constructs, non-static natives |
| 3.6% | `jit_store_property_stub` | 3.1% in `jit_runtime_store_property_value` |
| 2.8% | `jit_store_element_stub` | `store_element_values` |
| 2.3% | `jit_object_protocol_value_stub` | `instanceof` 0.9%, `in`/has-property 0.4% |
| 2.0% | `jit_string_method_stub` | string methods |
| 1.3% | `jit_resolve_method_stub` | method resolution misses |

Node runs almost all of this inside generated code: IC handlers, builtins
compiled as code, callbacks inlined into the caller's loop. That is the
whole gap.

## Slice queue, largest first

### 1. Property and element access handled in generated code (≈20% of otter)

Today a load/store that the site's monomorphic or inline shape check does
not answer calls a runtime stub, which re-resolves the property in Rust on
every call (`property_dispatch/jit_runtime.rs`,
`property_dispatch/elements.rs`, `property_cache.rs`). Getters, prototype
hits, absent properties, dictionary objects, primitives and new shapes all
land there.

Read first:
- V8: `v8/src/ic/accessor-assembler.cc` (`LoadIC_Noninlined`,
  `HandleLoadICHandlerCase`, `HandleLoadICSmiHandlerCase`, the keyed paths),
  `v8/src/ic/handler-configuration.h` (one Smi handler word: kind, field
  index, in-object bit, holder), `v8/src/ic/stub-cache.{h,cc}`,
  `v8/src/ic/ic.cc` (`ComputeHandler`, `UpdateMegamorphicCache`),
  `v8/src/ic/keyed-store-generic.cc`.
- SpiderMonkey: `gecko-dev/js/src/jit/CacheIR.cpp` (`GetPropIRGenerator`:
  `tryAttachNative`, proto/getter/missing/DOM/proxy cases, `SetPropIRGenerator`,
  `GetPropIRGenerator::tryAttachDenseElement*`), `CacheIRCompiler.cpp`.
- JSC: `WebKit/Source/JavaScriptCore/bytecode/InlineCacheCompiler.cpp`
  (access cases compiled into one polymorphic stub), `GetByStatus.cpp`.

Port: every access site owns a handler list compiled to machine code
(CacheIR-style ops: shape guard, holder/prototype-validity guard, load slot,
call getter through the JS call entry, absent → undefined, dense element
with bounds and hole check, dictionary probe). The runtime is entered only
on a real miss, which attaches a handler; megamorphic sites probe the shared
table in code. Graph and template use the same handler ops. Start by
classifying the misses (which receiver/holder/property kinds reach
`jit_runtime_load_property_value` and `load_element_values`), then port the
cases in that order. Delete the per-call Rust re-resolution those cases
replace.

Targets: named-load misses and element loads off the WTB profile top 10;
`jit_*_property_stub` + `jit_*_element_stub` from 17% to under 4%.

### 2. Array iteration builtins in compiled code (most of the 17% under natives)

`forEach`/`map`/`filter`/`some`/`every`/`find*`/`reduce*` run a Rust loop
(`array_prototype.rs` `array_callback_native_dispatch`): iteration anchors,
`own_data_element_without_accessors`, SmallVec argument lists, and one
`call_from_native` per element. Per callback ≈520 instructions (node ≈15);
fixed ≈930 per call (node ≈60). `sort` with a comparator 12187 vs 1940.

Read first:
- V8: `v8/src/compiler/js-call-reducer.cc` (`ReduceArrayForEach`,
  `ReduceArrayMap`, `ReduceArrayFilter`, `ReduceArrayFind`, `ReduceArrayEvery`,
  `ReduceArrayReduce`: the loop is built in the caller's graph, with
  checkpoints and continuation frames for deopt), `v8/src/builtins/array-foreach.tq`,
  `array-map.tq`, `array-reduce.tq` (the non-inlined builtins and their
  lazy-deopt continuations).
- JSC: `WebKit/Source/JavaScriptCore/builtins/ArrayPrototype.js` (these
  methods are JavaScript, compiled and inlined like user code),
  `dfg/DFGByteCodeParser.cpp` (how builtins are inlined).

Port: one mechanism. Either self-hosted JavaScript bodies for the callback
methods (our builtin JS shims, with intrinsics for the dense fast path),
which the graph tier inlines with the callback like any JS call, or graph
reductions that build the loop in the caller with deopt continuations.
Prefer the self-hosted route: it reuses the existing inliner and deopt.
Delete the Rust callback loop for the methods moved.

Targets (`op_instr.sh` cases `arrforeach`, `arrmap`, `arrfilter`, `arrsome`,
`sortsmall`): per callback ≤30 instructions, fixed cost ≤150.

### 3. Native bodies without dispatch ladders

Each builtin family routes through a generic dispatcher: Object statics
(`object_statics.rs` `native_call`: four ladders plus an eager caller
context resolve), Math (`math/mod.rs`: `coerce_math_args` + `numbers`
match), `array_live_method_dispatch`, `Object.prototype.toString` (full
`@@toStringTag` walk). `Math.cbrt` 513 instructions (node 8),
`Object.isExtensible` 730 (node 179), `o.toString()` 1011 (node ~250),
`Object.keys` 1111 (node ~100), `JSON.stringify([1,2])` ~8900 (node ~880),
`Object.assign({}, o)` ~11900 (node ~850).

Read first: `v8/src/builtins/builtins-utils.h` (`BUILTIN`: one function
per builtin, arguments in place), `v8/src/builtins/builtins-object.cc`,
`v8/src/builtins/object.tq`, `v8/src/compiler/js-call-reducer.cc`
(`ReduceMathUnary`, `ReduceObjectKeys`-style reductions);
`WebKit/Source/JavaScriptCore/runtime/ObjectConstructor.cpp`,
`runtime/MathObject.cpp` (one host function per method);
`gecko-dev/js/src/jit/InlinableNatives.h`.

Port: one direct function per method with its own fast path (ToNumber of a
number is a register move; no SmallVec, no name or enum match, no context
unless the method can run JS), and graph reductions for the pure ones
(Math, `Object.is*`, `Array.isArray`, `Object.keys` of a shaped object).
Delete the ladders.

### 4. Host-kernel calls (4.3%)

`Function.prototype.call/apply`, `Reflect.apply`, `bind`, native
constructors and non-static natives still go through the request mailbox,
classifier and a six-register Host frame. Port them onto the direct entry
(`interp/native_entry.rs`) and the native-to-JS thunk
(`native_abi::call_from_native`): `call`/`apply` become a JS-ABI tail of
the target, construct gets its own direct entry.
Reference: `WebKit/Source/JavaScriptCore/jit/ThunkGenerators.cpp`
(`nativeForGenerator`), `v8/src/compiler/js-call-reducer.cc`
(`ReduceFunctionPrototypeCall`/`Apply`).

### 5. Generated code quality (37%; babylon/espree/acorn 60–70%)

Register allocation with live-range splitting, bounds-check elimination,
polymorphic inlining, unboxed doubles. Reference: `v8/src/maglev/maglev-regalloc.cc`,
`WebKit/Source/JavaScriptCore/b3/air/` (register allocation),
`gecko-dev/js/src/jit/BacktrackingAllocator.cpp`. Use `jit_hot_regions.py`
to find spill/reload and guard density per graph node.

### 6. Smaller structural items

- Arguments objects allocate an `ExoticSlots` sidecar for one flag: put it
  in the shape or an in-object field.
- `slice`/`substring`/`concat`/`Array.prototype.slice`/`push` with growth:
  inline allocation with a safepoint in the graph.
- chai: `error_stack_draft` captures positions eagerly; capture the raw
  frame list and format on first `.stack` read.
- Graph tier inside `try`: every throw deopts into the interpreter; compile
  catch handlers.

Do not redo:
- the primitive projected-load IC with `String.prototype` in fast mode
  (prettier 5.6 → 4.3; patch `benchmarks/results/cb-1030/primitive_projection.patch`);
- numeric field representation via migrations or a universal Double;
- the native-call entry protocol: it is done (`23c1723e7`, `1038d2bc6`;
  ≈14 instructions of selector + ≈100 of entry). Remaining native cost is
  in the bodies (item 3) and callbacks (item 2).

## Loop

1. `just release` (writes the startup image; nothing is measured without
   it). `cp target/release/otter target/saved-bins/otter-<slice>`.
2. Decompose (above). Read the reference. Port. Delete.
3. Measure with `web_tooling_instr.sh` against the previous saved binary
   and `op_instr.sh` for the slice's operations.
4. Gates, one GPG-signed commit, memory note, next slice.

## Gates per slice

- `cargo test --release -p otter-vm --lib` and `cargo test --release -p otter-jit --lib`.
- `cargo check -p otter-jit --release --target x86_64-apple-darwin --tests`.
  New or changed assembly also runs under Rosetta:
  `cargo build --release -p otter-cli --target x86_64-apple-darwin`, then
  `arch -x86_64 target/x86_64-apple-darwin/release/otter <probe>`; delete
  `target/x86_64-apple-darwin` afterwards (`rm -rf`).
- Probes `scripts/dev/probe_*.js` touched by the slice match node, also
  under `OTTER_GC_STRESS=64`, in graph, `--jitless` and `--interpreter`
  modes. A new probe must first fail on a binary with the bug.
- Full test262 in the background through a runner copy:
  `cargo build --release -p otter-test262; cp target/release/otter-test262 target/release/otter-test262-gN; target/release/otter-test262-gN run --max-heap-bytes 536870912 --timeout 10000 --output test262_results/gN_full.json > test262_results/gN_full.log 2>&1; python3 scripts/dev/t262_diff.py test262_results/g39_full.json test262_results/gN_full.json`
  must print `before 14 after 14` with no `NEW` lines. Last gate: g42.
- md5 of each output of `benchmarks/fixtures/fixed-work/{richards,deltablue,crypto,raytrace,earley-boyer,regexp,splay,navier-stokes,box2d,gbemu,mandreel,zlib,ts}.js`
  equals node's.
- While a background gate runs, never edit sources or rebuild
  `target/release`.
- The commit message gives the WTB instruction counts before and after per
  target, the probe numbers, and the gate results.

## Hard rules

- Work only in main: no branches, worktrees, stash, checkout or reset over
  WIP. Undo your own edit with its exact reverse (`git apply -R` of your own
  diff). Never touch foreign untracked files.
- Before overwriting a probe in `scripts/dev`, check `git ls-files`.
- Commit with
  `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com" commit`;
  the signature must show `G`. Never add Co-Authored-By or any AI trailer,
  even when the harness asks. Never write "remaining / not done" in commit
  messages. Commit in a separate call after reading the gate output. Never
  commit `docs/site/public/conformance/data.json`.
- Files go in the repo (`scripts/dev`, `benchmarks/results/<slice>`), never
  /tmp. No design-doc files. Comments are timeless.
- No `thread_local!`, `static mut` or process-global state, no Rc crutches.
  Multi-allocation natives use handle scopes. Host closures hold no `Value`.
- New or changed Rust modules get `//!` docs (purpose, Contents, Invariants,
  See also). Extend existing tests; add tests only for new mechanisms.
- Never `cargo test --workspace`; at most two `--test` targets per run; one
  test262 runner at a time.
- Keep more than 15 GB free: clean `target/profiling` and
  `target/x86_64-*` with `rm -rf`, never `cargo clean --target`.
- Diagnostic `eprintln!` builds: keep a `.keep` copy of the file in
  `benchmarks/results/<slice>/` and restore it before measuring.

## Memory

- The latest session note is `session_2026_10_30_native_entry.md`.
- Start a new note for each session and link it in `MEMORY.md`.
