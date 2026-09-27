# Otter frontend compiler and bytecode: closures, environments, bytecode storage

Read-only review. Line numbers refer to the current working tree. The frame-cell batch work in `call_ops.rs`, `frame_state.rs`, `upvalue.rs` and `heap.rs` is uncommitted; I say "WIP" wherever it matters.

## 1. Components and key files

- **Parser and entry points.** `crates/otter-compiler/src/lib.rs:1-40` lowers the oxc AST to `BytecodeModule` in one pass per function. There is no intermediate IR.
- **Per-function state.** `FunctionContext` (`function_context.rs:21-212`) holds scopes, a scratch register counter, `captured_names`, `own_upvalue_count`, `parent_captures` and `captured_uv`.
- **Nested functions.** `Compiler` is a stack of contexts (`compiler.rs:23`). `resolve_capture` (`compiler.rs:313-365`) walks ancestor frames and adds a passthrough capture to every intermediate frame.
- **Capture pre-pass.** `capture.rs:35-55` computes `analyze_function` as own names ∩ names referenced from nested functions. Sets are keyed by name (`HashSet<String>`), not by scope binding. A nested direct eval promotes all own names (`capture.rs:50-53`).
- **Binding storage.** `scope.rs:57-66` has `BindingStorage::{Register{reg}, Upvalue{idx}}`. `allocate_own_upvalue` (`function_context.rs:300-315`) gives a new own slot to *every* declaration of a captured name.
- **Function lowering.** `functions.rs:30-410` does the arguments classification, eval promotion, prologue, self-name handling and finalisation.
- **Closure emission.** `emit_make_callable` and `make_closure_operands` (`functions.rs:789-853`) emit the closure op. The limit is 252 captures (`functions.rs:833`).
- **Hoisting.** `hoist.rs:185-209` handles captured `var` bindings, `hoist.rs:327-363` top-level lexicals, and `hoist.rs:371-402` block lexicals (these use `FreshUpvalue`).
- **Arguments elision.** `arguments_elision.rs:44-175` is a forward dataflow over lowered wordcode. It is invoked at `functions.rs:376-389`.
- **Bytecode crate.** It defines:
  - the `Op` enum (`lib.rs:74`); the byte table has 191 opcodes (`opcode_schema.rs:848`);
  - `Function` (`lib.rs:1966-2197`) and `Constant` (`lib.rs:2345`);
  - the schema rows `OpcodeSchema` (`opcode_schema.rs:788-813`): operand roles, successors, binding semantics, effects, tier policy;
  - the verifier (`verifier.rs:1-70`).
- **VM load.** `ExecutableModuleBuilder` turns each `Function` into a `CodeBlock` (`executable.rs:1005-1105`, `1107-1265`). `CodeBlockControlFlow` (`code_block_cfg.rs:1-80`) precomputes blocks, loops and exception regions.
- **Runtime closure objects.** `upvalue.rs`, `closure.rs`, `upvalue_source.rs`, `function_ops.rs:163-300`, `frame_ops.rs:56-149`, and `call_ops.rs:1143-1231` (generated-code spine initialisation).
- **Compile cache.** `crates/otter-runtime/src/compile_cache.rs` (BLAKE3 key plus build fingerprint) and `crates/otter-bytecode/src/binary.rs` (flat format).

## 2. Data layouts

| Record | Size | Fields / source |
|---|---|---|
| `wordcode::Instruction` (compiler and cache form) | 24 B | `op` u8, `operand_count` u8, `[u32;4]` inline words, `overflow_operand_offset` u32. More than 4 words spill to a per-function dense table (`wordcode.rs:33-47`) |
| `CodeBlockInstruction` (VM execution form) | 32 B | `instruction_pc` u32, `property_ic_site` u32, `code_block_id` u32, `[u32;4]`, `op`, count, `reductions`, reserved (`executable.rs:1347-1369`) |
| Flat cache record | 22 B per instruction | u8 op, u8 count, 4×u32, u32 overflow (`binary.rs:446-468`) |
| Cold byte stream | variable | op u8, count u8, then per operand: kind u8 + u16 (register) or u32 (`encoding.rs:33-46`). Used only for byte PCs and spans |
| `UpvalueCellBody` | 8 B GC header + 8 B `Value` = 16 B | `upvalue.rs:51-56`, header `header.rs:124-129`. Census check: 1,774,406,656 B / 110,900,416 cells = 16 |
| `ClosureCallHeader` | 24 B | `function_id`, `flags`, `upvalue_base` (raw u64 address), `upvalue_count`, `eval_env` (`closure.rs:97-107`, asserts at `:338-347`) |
| `JsClosureBody` | 112 B including header, plus a 4 B × n capture tail | Census before the tail change: 1,141,647,696 / 10,193,283 = 112.0. The tail sits right after the body (`closure.rs:391`) |
| `NativeFrame` | 72 B | `upvalue_base` @24, `upvalue_count` @56, `eval_env` @60, `argument_count` @64 (`native_abi/frame.rs:192-221`, `422-426`) |
| Interpreter `Frame` | 72 B | Includes `upvalues: Box<[UpvalueCell]>` (`frame_state.rs:61,73,127`) |

- **`MakeClosure` operands:** `dst`, `fn_const`, `ConstIndex(count)`, then one `Imm32(parent_idx)` word per capture (`functions.rs:844-851`). With 2 or more captures (3 + n > 4 words) the operands always live in the overflow table.
- **Frame spine order:** own cells first (`0..own`), then inherited cells (`lib.rs:1985-1996`). Parent-capture indices are issued in a virtual space starting at 0x8000 and rewritten to `own + k` when the function is finalised (`function_context.rs:1067-1127`).

## 3. Hot paths

### 3.1 Compile pipeline (per function, `functions.rs:30-410`)

1. **Decide eval and arguments facts.** Scan for direct eval (`:49`). Classify `arguments` usage: needed, mapped, forward-only, legacy `.arguments` (`:55-78`).
2. **Choose captured names.** Run the name-level capture analysis (`:104`). Add the self-name if nested code references it, and add `arguments` if needed. Direct eval adds all own names (`:139-144`). Reserve own slots for these names in sorted order (`:151`, `function_context.rs:278-296`).
3. **Lower in one pass.** Registers are a bump counter starting at `param_count`, reset only at expression level (`function_context.rs:317-362`). `slot.locals = 0` (`:396`), so every binding and temporary is "scratch".
4. **Finalise.** Rewrite virtual capture indices (`:367`). Run arguments elision if eligible: zero formals, no rest, no eval, not async and not a generator (`:376-389`).

### 3.2 Call prologue for a function with own captured bindings

**Interpreter** (`frame_state.rs:495-526`):
- `own == 0`: `copy_owned()` makes a host `Vec` copy of the inherited handles on *every* call (`upvalue_source.rs:124-133`).
- `own > 0`: a host `Vec` of `own + inherited`, then a batch old-space allocation of `own` cells (WIP; the committed version made one `alloc_upvalue_with_roots` call per cell), then the inherited handles are pushed and the `Vec` becomes a boxed slice.
- The `Box` is freed at return.

**Generated direct call** (`arm64/direct_call.rs:1482-1515`):
- `own == 0`: the callee's `upvalue_base` points straight at the closure tail. No transition, no copy (`:1449-1476`).
- `own > 0`: runtime stub 82 is called, which runs `jit_initialize_generated_upvalues` (`call_ops.rs:1143-1231`). It calls `context.for_function` and `exec_function`, checks the counts, runs `collect_allocation_roots` (an empty `Vec` when providers are registered, `allocation_ops.rs:54-56`), batch-allocates the cells, then copies each inherited handle out of the closure into the stack window.

**Prologue bytecode after the cells exist:**
- A captured parameter gets `StoreUpvalue` from its argument register.
- A captured `var` gets `LoadUndefined` + `StoreUpvalue` (`hoist.rs:200-205`), even though the batch already set the cell to `undefined`.
- A captured top-level lexical gets `LoadHole` + `StoreUpvalue` (`hoist.rs:350-359`).
- A captured block lexical gets `FreshUpvalue` on every block entry (`hoist.rs:395-400`). Its own slot was also counted in `own_upvalue_count` (`function_context.rs:310-314`), so the prologue cell is allocated and then immediately replaced.
- A captured `for (let …)` head costs `LoadUpvalue` + `FreshUpvalue` + `StoreUpvalue` per iteration (`statements.rs:1346-1354`).

### 3.3 `LoadUpvalue` / `StoreUpvalue`

**Interpreter:**
- Each tick: frame-cache check, fetch the 32 B record, `record_work`, hooks branch, `match` (`interp/dispatch.rs:118-252`).
- Then `frame_load_upvalue` (`frame_ops.rs:56-69`): bounds-checked handle read, decompress and read the payload (`heap.rs:1749-1760`), hole check, bounds-checked register write, PC advance.
- My estimate (not measured): 40–60 host instructions.
- The store path adds `store_upvalue`, which calls the `record_write` barrier (`upvalue.rs:108-114`, `heap.rs:2513-2521`).

**Template (arm64)** (`template/arm64/binding.rs:257-326`):
- Read: `ldr upvalue_base; cbz; ldr w handle; cbz; mov-wide cage base (≤4, GcCageBase reloc); add; ldr value; mov-wide HOLE (≤4); cmp; b.eq miss; str reg`. About 12–18 instructions, three dependent loads.
- Store: same prefix, then `emit_cell_store` (cell-type test, `str`, inline write barrier; `:108-131`). About 20–28 instructions.
- Misses go to a committed cold call.

**Machine** (`machine/numeric/arm64.rs:784-825`):
- `ldr frame; ldr upvalue_base; ldr w handle; mov-wide cage; add; add`, plus a hole check when `binding_requires_live_cell`. About 10–16 instructions, four dependent loads.
- An inlined callee reads through the proved closure's `upvalue_base` instead (`ClosureUpvalue`, `hir.rs:183`).

### 3.4 `MakeClosure`

**Interpreter** (`dispatch.rs:2686` → `function_ops.rs:163-300`):
- Decode the overflow operands into a `SmallVec<[u32;16]>`; `frame_cold` lookup.
- `make_closure_value` then:
  - resolves `function_id_constant` (constant-pool `Vec` index plus enum match);
  - checks for self (`:263`);
  - copies cells into a `SmallVec<[UpvalueCell;16]>`;
  - calls `function_is_arrow` three times (`:272-285`, each an `exec_function` lookup);
  - calls `alloc_closure`, which goes to `alloc_variable_with_roots_initialized` (old space) and then copies the tail and runs `refresh_upvalue_base` (`closure.rs:779-819`);
  - runs `mark_closure_lookup`, which calls `function_kind_prototype_for` → `for_function` + function lookup (`function_kind.rs:303-315`) and possibly writes the heap.

**Template:**
- About 12 instructions of setup, then transition stub 22 with a raw pointer to the parent-index slice in the plan arena (`template/arm64/transitions.rs:144-178`).
- On the Rust side: `jit_make_closure_stub` → `runtime_call()` → `jit_runtime_make_closure` (`jit_runtime_ops.rs:266-298`), which does `for_function` plus everything the interpreter path does, and saves/restores the PC.

**Machine:**
- The committed-value path re-decodes `published_const_index` ×2 and `published_imm32` once per capture on every execution, then calls `make_closure_value` (`committed_values.rs:607-645`).

**`MakeFunction` without captures** still allocates a new closure per evaluation to preserve identity (`function_ops.rs:138-160`).

Per-closure-creation cost cannot be split from the sample profile. My speculation is several hundred host instructions per closure, which fits the `make_closure_value` 127+36 samples.

### 3.5 Arguments, generators/async, eval

- **Arguments.**
  - `CollectArguments` materialises the object.
  - Mapped arguments force simple formals into cells (`functions.rs:148-150`).
  - `CallForwardArguments` handles pure `apply` forwarding (`lib.rs:325-336`).
  - After elision, `LoadArgumentsLength`/`LoadArgumentsElement` read the actual-argument window (`lib.rs:698-706`).
- **Generators and async.**
  - `GeneratorStart` runs after the prologue.
  - Each suspension copies the whole register window into `OwnedRegisterSnapshot(SmallVec<[Value;8]>)` plus a boxed upvalue spine (`frame_state.rs:78,158-166`).
  - There is no state-machine lowering.
- **Eval.**
  - Sloppy eval between a binding's owner and its use turns reads into `LoadShadowedUpvalue*` with an eval-depth operand (`compiler.rs:397-423`).
  - An eval-containing function forces passthrough captures of up to 200 visible cell names (`functions.rs:868-903`).
  - `EvalEnvBody` stores `names: Vec<String>` and `cells: Vec<UpvalueCell>` (`eval_env.rs:40-50`).
  - Eval bodies are compiled at runtime (`eval_ops.rs:861-866`) and are not cached.

### 3.6 Compile cache

- Key: BLAKE3 over source, kind, specifier and the build fingerprint (`compile_cache.rs:14-20,238-245`).
- It is used only for bootstrap scripts and the realm installer (`otter-runtime/src/lib.rs:5683-5713`, `realm.rs:191-205`). Ordinary scripts are deliberately not cached (`lib.rs:5684-5686`).
- A cache hit runs the full verifier (`binary.rs:17-33`).
- Only the `BytecodeModule` is persisted. `CodeBlock`, CFG, feedback and JIT code are rebuilt in every process.

## 4. Invariants

- **Address stability.**
  - Binding cells are old-space and address-stable: closure tails and frame windows trace slots in place, and global-lexical proofs embed cell addresses (`upvalue.rs:73-75`).
  - Closures are allocated in old space (closure-captures report, lines 29-34). `UpvalueSource` needs an allocation that never moves while it is borrowed (`upvalue_source.rs:4-13`).
  - `NativeFrame.upvalue_base` and `ClosureCallHeader.upvalue_base` are raw process addresses. Heap-image restore recomputes the header value (`closure.rs:239-247`).
- **Rooting.**
  - Pending closure bodies trace fixed fields only; the source capture buffer is rooted by the caller until the tail is initialised (`closure.rs:249-253,797-817`).
  - Generated spine initialisation keeps `upvalue_count = 0` until the batch completes, then re-reads SELF after any collection before copying inherited cells (`call_ops.rs:1169-1231`).
- **TDZ.** A hole in a cell means TDZ. Every tier checks it on reads (and on checked stores). Register bindings get compile-time TDZ instead (`scope.rs:33-38`, `assignment.rs:384`).
- **Loop and block identity.** `FreshUpvalue` gives per-iteration and per-block-entry binding identity. Closures made earlier keep the old cell (`lib.rs:296-306`).
- **Verified operands.** Every operand and index is bounds-checked once at admission. Hot accessors do not re-validate (`executable.rs:22-30`, `verifier.rs:14-22`).
- **Deopt and safepoints.** Stubs 22, 25 and 82 are declared allocating with status exceptions (`runtime_stubs.rs:657-713`). Exact deopt rebuilds only catch-only handler stacks; `finally` regions are rejected (`code_block_cfg.rs:15-19,50-66`).

## 5. Duplication and transitions that could be collapsed

1. **Six bytecode representations.**
   - Compiler wordcode, 24 B (`wordcode.rs:38`).
   - VM `CodeBlockInstruction`, 32 B: a full copy in which 8 of 32 bytes are redundant. `instruction_pc` always equals the index and `code_block_id` is constant within a block (`executable.rs:1347-1369`).
   - The cold byte stream, kept only to compute `byte_pcs` and `byte_spans` (`executable.rs:1083`).
   - The flat 22 B cache format.
   - The serde JSON dump.
   - The decoded `Instruction` DTO (`lib.rs:1934`).
2. **Two PC spaces.**
   - The interpreter uses instruction indices. The JIT snapshot keys programs and proofs by *byte* PC (`jit.rs:290,1483`).
   - The Template binding emitter looks up each binding op's immediate with a linear `find` over all instructions (`binding.rs:51-55`), which is quadratic per function. Speculation: this contributes to the long compile times (zlib about 3.8 s).
3. **Many upvalue spine forms.**
   - The closure inline tail.
   - The interpreter `Frame.upvalues` `Box`.
   - The native stack window plus direct-call copy.
   - The parked-frame `Box`.
   - `UpvalueSource`.
   - `EvalEnvBody.cells`.
   - Inherited handles are copied into every activation that has own cells, and into a host `Box` on every interpreted closure call.
4. **Three spine builders:** interpreter (`frame_state.rs:495-555`), generated (`call_ops.rs:1143`), and `FreshUpvalue` (`frame_ops.rs:141-149`).
5. **Static facts recomputed at runtime.** `is_arrow` (×3), the kind prototype, the function-id constant and chunk resolution are looked up on every closure creation, although all of them are fixed per `MakeClosure` site.
6. **Wasted or duplicated cell initialisation.**
   - Prologue cells for block lexicals are immediately replaced.
   - Captured `var` cells get a redundant `undefined` store.
7. **`LoadLocal`/`StoreLocal` are plain register moves** with an `Imm32`-typed register operand, because locals are registers (`function_context.rs:905-922`).
8. **Name-keyed capture over-promotes.** Every same-named declaration in a function becomes a cell if any one of them is captured (`function_context.rs:305-309`). How much this costs was not measured.
9. **Function flags stored twice.** `Function` flags are copied into `CodeBlock` (`executable.rs:1217-1260`), and both are held for the lifetime of the chunk.

## 6. What would have to change

### (a) A much cheaper hot path

1. **Capture analysis per binding, with mutation analysis.**
   - Do the analysis per scope-resolved binding, not per name, and record whether each captured binding is written after its dominating initialisation.
   - Immutable-after-init captures would be copied by value into the closure tail, either as an 8 B `Value` or as the 4 B tagged slot proposed in closure-captures.md:158-166. The owner would keep them in registers.
   - Only mutable, eval-visible or mapped-arguments bindings would keep cells.
   - Evidence: in `deriv_trees`, 8 of 10 own bindings are parameters stored once (closure-captures.md:148-156). Needs an initialisation-order proof (closure-captures.md:168-173).
   - Speculation: this removes most of the 110.9M cells, their barriers, and the remembered-set traffic that stores into old-space cells create.
2. **Allocate block-lexical cells only at block entry.** Emit per-scope own-cell ranges so the prologue batch covers only function-scope cells.
3. **Make `MakeClosure` static.**
   - Put the global function id, arrow/kind bits and parent list in a `CodeBlock` side table resolved at load.
   - In the JIT, allocate inline (bump in an old-space LAB), copy the tail in a loop, and set the header directly. Stub 22 would remain only as a slow path.
4. **Two-base upvalue addressing.** Use an own window plus the closure tail, and stop copying inherited handles into activations. Replace the interpreter `Box` spine with an arena window, the same way registers are handled.
5. **Unchecked `LoadUpvalue`.** Emit it where initialisation dominates the read (owner reads after init; closures created after init). The compiler already tracks `initialized` for register bindings.
6. **Liveness-based register reuse across statements** to shrink windows and snapshot sizes (lower priority).

### (b) A portable AOT artifact

- **Bytecode is already portable.** The flat format carries no addresses, function ids are rebased by chunk base (`verifier.rs:14-22`), constants are referenced by index, IC site ids are assigned at load (`executable.rs:1123-1136`), and the build fingerprint covers layout constants.
- **Native code bakes process-local values.** The relocation catalogue at `artifact/relocation.rs:109-156` lists them, but it is capture-only and "Code bytes are runtime-local" (`jit_artifact.rs:21-24`):
  - cage base
  - runtime-stub addresses
  - template operand-slice pointers (`transitions.rs:164`)
  - global lexical, string and property-source cells
  - lookup-cache tables
  - direct-call entry cells
  - guarded heap references
- **Changes needed for AOT code:**
  - hold the cage base in a pinned register;
  - call runtime stubs through an indirection table;
  - reference heap constants through a per-module load-time table indexed by id;
  - key everything by logical PC;
  - make feedback speculation either absent or symbolic, guarded, and patched at load.
- **Facts a native compiler can reuse today:**
  - opcode schema: register roles, effects, successors, binding semantics;
  - `CodeBlockControlFlow`;
  - own/inherited counts and `MakeClosure` parent maps;
  - arguments plans;
  - `contains_direct_eval` / `observes_eval_env` / `makes_function`;
  - TypeScript number and class hints;
  - spans.

## 7. Open questions

1. Does the Machine tier CSE or hoist the upvalue handle or cage decompress across loop iterations? I did not check its LICM.
2. How are Earley's 110.9M cells split between prologue batches, `FreshUpvalue` and per-iteration copies? The census does not separate them.
3. How much does name-keyed over-promotion cost in the workloads?
4. How is the `make_closure_value` time divided between lookups and allocation?
5. Is old space ever compacted? The address-stability comments say no, but I did not verify it in `otter-gc`.
6. Will the WIP frame-cell batch change the numbers? It has not been measured (frame-cell-batches.md:44-48).
7. Does the byte-PC linear lookup matter for zlib's 3.8 s compile time? Not profiled.