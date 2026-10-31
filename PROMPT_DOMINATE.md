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

otter does 1.96x node's instructions on WTB. That gap is architecture, not
tuning. Every slice:

1. **Decompose first.** Measure where otter's instructions go for the
   subsystem (profile shares, per-operation counts against node). Write the
   numbers down in the memory note.
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
  target, so a whole run is fixed work; other sessions on this machine make
  wall time swing 2–3x):
  `scripts/dev/web_tooling_instr_one.sh <bin|node> <out> [target...]`, then
  `scripts/dev/instr_ratio.py <after> <before>` (per target + geomean).
  Node's counts: `benchmarks/results/dom0/node.txt`. Whole-run counts
  include bundle parse/compile; compare like with like. Noise is ±1–2%;
  repeat a target 3 times and take the median before calling a small move.
- Wall clock, when the machine is quiet (`uptime`, load < 3):
  `scripts/dev/web_tooling_matrix.sh <bin> <out>`.
- Per-operation instructions against node and bun:
  `scripts/dev/op_instr.sh <bin> [N] [case...]`, and for property/element
  access classes `scripts/dev/access_classes_instr.sh <bin>` (tlen, strchar,
  intkey, intkeydict, intkeyset, dictload, dictprotoget, dictprotoabsent,
  megastr, wmget, fncall, holeload, symload). Probes must run inside
  functions; top-level loops take other paths.
- Profiles: `scripts/dev/web_tooling_profile.sh <bin> <dir>`, then
  `scripts/dev/web_tooling_hot.py <dir> self|exits N`; drill down with
  `jit_exits.py`, `children_of.py`, `caller_tier.py` (needs
  `--perf-basic-prof`), `jit_hot_regions.py` (self time per graph node),
  `isolate_self.py`, `sample_isolate.sh`.
- Runtime-miss census: `benchmarks/results/dom0/census.patch` (+
  `access_census.rs.diag`, `census_agg.py`) is a diagnostic build that counts
  receiver/holder/property kinds reaching the runtime access paths. Apply it
  with `patch -p1`, keep `.keep` copies, restore before measuring.
- Octane: `scripts/dev/wallclock_matrix.sh target/release/otter benchmarks/results/<slice>/m 5`.

## Standings (2026-10-31, access slice in the working tree)

WTB retired instructions, whole run, otter / node
(`benchmarks/results/dom0/otter1c.txt` vs `node.txt`):

| target | otter | node | x |
|---|---|---|---|
| coffeescript | 125.4G | 42.6G | 2.95 |
| lebab | 99.8G | 35.3G | 2.83 |
| chai | 77.6G | 29.0G | 2.68 |
| babel-minify | 84.8G | 32.6G | 2.60 |
| uglify-js | 97.2G | 38.0G | 2.56 |
| buble | 84.8G | 37.7G | 2.25 |
| jshint | 67.3G | 30.4G | 2.22 |
| postcss | 88.4G | 43.2G | 2.05 |
| acorn | 78.5G | 38.5G | 2.04 |
| babel | 88.3G | 43.9G | 2.01 |
| prettier | 93.0G | 48.2G | 1.93 |
| prepack | 85.4G | 48.9G | 1.75 |
| esprima | 65.0G | 37.6G | 1.73 |
| espree | 62.8G | 37.6G | 1.67 |
| source-map | 76.4G | 46.0G | 1.66 |
| typescript | 57.3G | 39.7G | 1.44 |
| terser | 36.6G | 28.9G | 1.26 |
| babylon | 41.2G | 41.5G | 0.99 |

Geomean 1.96x (2.16x at HEAD ccfb0627b; the access slice is 0.924 of HEAD).
Octane matrix 1.43x node, 1.80x bun (2026-10-30).

### Where otter's time goes (WTB mean of 18 profiles at HEAD, before the access slice)

| share | where | what it is |
|---|---|---|
| 37% | generated code | compiled JS itself |
| 17% | `native_exit_entry` | bodies of natives called from JS, including the Rust loop of callback methods |
| 6.0% | `jit_load_property_stub` | named-load IC misses |
| 4.9% | `jit_load_element_stub` | element loads |
| 4.3% | `otter_native_invoke` | Host-kernel calls: `call`/`apply`/`bind`, constructs, non-static natives |
| 3.6% | `jit_store_property_stub` | named-store misses |
| 2.8% | `jit_store_element_stub` | element stores |
| 2.3% | `jit_object_protocol_value_stub` | `instanceof`, `in` |
| 2.0% | `jit_string_method_stub` | string methods |
| 1.3% | `jit_resolve_method_stub` | method resolution misses |

Re-profile after the access slice lands; the access rows shrank.

## Step 0: land the access slice in the working tree

Uncommitted, measured, gates not run (memory note
`session_2026_10_31_access_slice.md`): Int32 keys → atoms through the
isolate integer-key cache read by generated code (`VmThread.index_atoms`),
`str[i]` → shared one-unit strings (`VmThread.unit_strings`,
`arm64|x86_64/string_unit.rs`), the typed-array eager-expando bug in named
[[Set]], and the typed-array `.length` shadowing bug (length protector +
`IcHandlerKind::TypedArrayLength`). Saved binaries: `otter-dom0` (HEAD),
`otter-dom1c` (slice).

Run the gates below (x86: Rosetta run of `probe_access_classes.js`, the
typed-length handler is only compile-checked), then one commit with the
WTB table (0.924 geomean; postcss 0.694, esprima 0.838, terser 0.856,
buble 0.858, babel 0.897) and the per-op numbers (intkey 805→94,
intkeyset 3122→126, intkeydict 845→152, strchar 574→31, Buffer blit
11288→841 per call).

## Slice queue, largest first

### 1. Why generated code costs 2x node's (37% of otter; 60–70% on babylon/espree/acorn)

node retires about half of otter's instructions, and the largest single
bucket is compiled JS itself. Nobody has measured why. Diagnose before
porting; the answer decides the slice.

Decompose, per target, for coffeescript, lebab, chai (worst ratios) and
acorn, babylon (generated-code heavy):

1. **Tier residency.** Share of samples in interpreter, template code and
   graph code (`caller_tier.py`, `--perf-basic-prof`). Count deopts and
   re-compiles per hot function; a function bouncing between tiers costs
   more than any codegen detail.
2. **Hot functions side by side.** For the top 10 graph functions by self
   time, instructions per invocation in otter vs node. Node:
   `node --print-opt-code --print-opt-code-filter=<fn>` and
   `--trace-opt --trace-deopt`; otter: the graph dump plus
   `jit_hot_regions.py` self time per graph node.
3. **Per-node anatomy.** Classify every instruction of those functions:
   work, shape/map guard, type guard, boxing/unboxing, spill/reload,
   move, call prologue/epilogue, safepoint poll, write barrier, frame
   state. Expected suspects: shape check ~10 instructions vs node's 4,
   save/restore of every live value around calls (no live-range
   splitting), Int32/double boxing at every merge, repeated guards on the
   same value, template code where node has optimized code.
4. Write the table (bucket × target, otter vs node) in the memory note,
   then pick the largest bucket as the port.

Read for the port once the bucket is known:
- Register allocation: `v8/src/maglev/maglev-regalloc.cc`,
  `WebKit/Source/JavaScriptCore/b3/air/AirAllocateRegistersByGraphColoring.cpp`,
  `gecko-dev/js/src/jit/BacktrackingAllocator.cpp`.
- Guards and maps: `v8/src/maglev/maglev-graph-builder.cc` (known node
  aspects, `BuildCheckMaps`), `v8/src/compiler/js-native-context-specialization.cc`.
- Representation: `v8/src/maglev/maglev-phi-representation-selector.cc`,
  `WebKit/Source/JavaScriptCore/dfg/DFGFixupPhase.cpp`.
- Tiering: `v8/src/execution/tiering-manager.cc`,
  `WebKit/Source/JavaScriptCore/jit/JITPlan.cpp`, `bytecode/ExecutionCounter.h`.

Target: generated-code instructions on the five targets within 1.3x of
node's for the same functions.

### 2. Remaining property/element access classes

The census (memory note) ranks what still reaches the runtime after the
access slice: dictionary-mode named loads ~25M (DictionaryField,
dictionary absent and prototype handlers in code), string receivers at
megamorphic sites 14.5M (`.type` on primitives), slab-growth store
transitions 10.4M (esprima `entry.range =` on a two-field literal),
WeakMap.get/has 5.7M, getter calls from handlers, `fn.call/apply` 3.9M,
the graph CacheIR program for TypedArrayLength.

Read: `v8/src/ic/accessor-assembler.cc` (`HandleLoadICHandlerCase`,
dictionary and proto cases), `v8/src/ic/handler-configuration.h`,
`v8/src/ic/stub-cache.cc`, `v8/src/ic/keyed-store-generic.cc`;
`gecko-dev/js/src/jit/CacheIR.cpp` (`GetPropIRGenerator`,
`SetPropIRGenerator` add-slot with realloc), `CacheIRCompiler.cpp`;
`WebKit/Source/JavaScriptCore/bytecode/InlineCacheCompiler.cpp`.

Port each class as a handler kind run by generated code (graph and template
share the ops); the runtime only attaches. Delete the per-call Rust
re-resolution for each class ported. Target: `jit_*_property_stub` +
`jit_*_element_stub` under 4% of WTB.

### 3. Array iteration builtins in compiled code (most of the 17% under natives)

`forEach`/`map`/`filter`/`some`/`every`/`find*`/`reduce*` run a Rust loop
(`array_prototype.rs` `array_callback_native_dispatch`). Per callback ≈520
instructions (node ≈15); fixed ≈930 per call (node ≈60). `sort` with a
comparator 12187 vs 1940.

Read: `v8/src/compiler/js-call-reducer.cc` (`ReduceArrayForEach`,
`ReduceArrayMap`, …, continuation frames), `v8/src/builtins/array-foreach.tq`,
`array-map.tq`, `array-reduce.tq`;
`WebKit/Source/JavaScriptCore/builtins/ArrayPrototype.js`,
`dfg/DFGByteCodeParser.cpp`.

Port: self-hosted JavaScript bodies with intrinsics for the dense path,
inlined with the callback by the graph tier like any JS call (preferred),
or graph reductions with deopt continuations. Delete the Rust callback loop
for the methods moved. Targets (`op_instr.sh` `arrforeach`, `arrmap`,
`arrfilter`, `arrsome`, `sortsmall`): per callback ≤30, fixed ≤150.

### 4. Native bodies without dispatch ladders

Object statics, Math, `array_live_method_dispatch`,
`Object.prototype.toString` route through generic dispatchers. `Math.cbrt`
513 (node 8), `Object.isExtensible` 730 (179), `o.toString()` 1011 (~250),
`Object.keys` 1111 (~100), `JSON.stringify([1,2])` ~8900 (~880),
`Object.assign({}, o)` ~11900 (~850).

Read: `v8/src/builtins/builtins-utils.h`, `builtins-object.cc`,
`object.tq`, `js-call-reducer.cc` (`ReduceMathUnary`);
`WebKit/Source/JavaScriptCore/runtime/ObjectConstructor.cpp`,
`runtime/MathObject.cpp`; `gecko-dev/js/src/jit/InlinableNatives.h`.

Port: one direct function per method with its own fast path, graph
reductions for the pure ones. Delete the ladders.

### 5. Host-kernel calls (4.3%)

`call`/`apply`/`Reflect.apply`/`bind`, native constructors and non-static
natives still use the request mailbox and a six-register Host frame. Move
them onto `interp/native_entry.rs` and `native_abi::call_from_native`:
`call`/`apply` become a JS-ABI tail of the target, construct gets its own
direct entry. Reference: `WebKit/Source/JavaScriptCore/jit/ThunkGenerators.cpp`,
`js-call-reducer.cc` (`ReduceFunctionPrototypeCall`/`Apply`).

### 6. Smaller structural items

- Arguments objects allocate an `ExoticSlots` sidecar for one flag.
- `slice`/`substring`/`concat`/`push` with growth: inline allocation with a
  safepoint in the graph.
- chai: `error_stack_draft` captures positions eagerly; format on first
  `.stack` read.
- Graph tier inside `try`: every throw deopts; compile catch handlers.
- A JIT throw's stack excerpt shows the caller's line while the frame names
  the callee (seen with `probe_access_classes.js` `writeKey`).

Do not redo:
- the primitive projected-load IC with `String.prototype` in fast mode
  (patch `benchmarks/results/cb-1030/primitive_projection.patch`);
- numeric field representation via migrations or a universal Double;
- the native-call entry protocol (`23c1723e7`, `1038d2bc6`);
- int-key atomization, `str[i]`, typed-array `.length` (access slice).

## Loop

1. `just release` (writes the startup image; nothing is measured without
   it). `cp target/release/otter target/saved-bins/otter-<slice>`.
2. Decompose. Read the reference. Port. Delete.
3. Measure with `web_tooling_instr_one.sh` + `instr_ratio.py` against the
   previous saved binary and the per-operation scripts.
4. Gates, one GPG-signed commit, memory note, next slice.

## Gates per slice

- `cargo test --release -p otter-vm --lib` and `cargo test --release -p otter-jit --lib`.
- `cargo check -p otter-jit --release --target x86_64-apple-darwin --tests`.
  New or changed assembly also runs under Rosetta:
  `cargo build --release -p otter-cli --target x86_64-apple-darwin`, then
  `arch -x86_64 target/x86_64-apple-darwin/release/otter <probe>`; delete
  `target/x86_64-apple-darwin` afterwards (`rm -rf`).
- Probes `scripts/dev/probe_*.js` touched by the slice match node
  (`probe_access_classes.js` → `benchmarks/results/dom0/pac.node.txt`,
  `probe_native_entry.js` → `pne.node.txt`), also under
  `OTTER_GC_STRESS=64`, in graph, `--jitless` and `--interpreter` modes.
  A new probe must first fail on a binary with the bug.
- Difftest of the slice's probes against `--interpreter`.
- Full test262 in the background through a runner copy:
  `cargo build --release -p otter-test262; cp target/release/otter-test262 target/release/otter-test262-gN; target/release/otter-test262-gN run --max-heap-bytes 536870912 --timeout 10000 --output test262_results/gN_full.json > test262_results/gN_full.log 2>&1; python3 scripts/dev/t262_diff.py test262_results/g42_full.json test262_results/gN_full.json`
  must print `before 14 after 14` with no `NEW` lines. Last gate: g42.
  Background Bash needs `timeout: 7200000`.
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
- No sudo, dtrace or other tools that prompt for a password.

## Memory

- The latest session note is `session_2026_10_31_access_slice.md`.
- Start a new note for each session and link it in `MEMORY.md`.
