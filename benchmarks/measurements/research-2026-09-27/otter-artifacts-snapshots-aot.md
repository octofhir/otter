# Serialization and code artifacts in Otter: compile cache, heap image, JIT artifacts, AOT readiness

## 1. Components and key files

| Component | Location | Role |
|---|---|---|
| Bytecode flat format | `crates/otter-bytecode/src/binary.rs:1-35,78-90` (`encode_module_bounded`, `decode_module`, magic `otterbc\0` :49) | Little-endian, length-prefixed, no version (:29-31). Decoding is budgeted (:52-55). Every decode is verified. |
| Compile cache | `crates/otter-runtime/src/compile_cache.rs:134-245` (`CompileCache`, `cache_key` :250) and `compile_cache/unix.rs:49-92` | Owner-private on-disk cache keyed by BLAKE3 over source, kind, specifier and a build fingerprint (`build.rs:1-50`). Used only for bootstrap scripts (`lib.rs:5683-5713`) and the realm installer (`realm.rs:188-206`). User scripts and modules are not cached (`lib.rs:5683-5686`). |
| Code space | `crates/otter-vm/src/code_space.rs:82-89` (`ChunkPayload`), `:361-392` (`build_chunk`), `:502-535` (`link_verified_module`), `:790-843` (`rebase_module`) | Rebases function ids into one per-interpreter id space and builds `ExecutableModule` plus `AtomTable`. |
| Heap image | `crates/otter-gc/src/heap_image.rs:133-157` (`PageImage`/`HeapImage`), `:261-286` (capture), `:306-487` (restore) | Copies old-space pages and relocates them page by page. In-process only (:39-41). |
| Self-containment audit | `crates/otter-gc/src/self_contained.rs:1-47`; allow-list in `crates/otter-runtime/tests/bootstrap_heap_census.rs:256-266` | Three body types still own storage outside the heap: `ExoticSlots`, `ArrayExoticSlots`, `JsRegExpBody`. |
| External and host refs | `otter-gc/src/external_refs.rs:1-80`, `host_refs.rs:1-60` | Dense per-isolate indices for Rust fn addresses and `Arc` payloads. |
| Isolate snapshot | `crates/otter-vm/src/snapshot.rs:64-104` (`IsolateSnapshot`), `interp/restore.rs:66-263`; runtime wrapper `otter-runtime/src/runtime_snapshot.rs:34-37` | The only production user is the test262 runner (`otter-test262/src/isolation.rs:79,95`). The CLI never restores from a snapshot. |
| JIT artifact bundle | `crates/otter-vm/src/jit_artifact.rs:1-110`, `crates/otter-jit/src/artifact.rs:1-35` | Diagnostic output only, default-off. The CLI enables it through `jit_artifacts_target` (`otter-cli/src/execution_config.rs:36,235-264`). |
| Relocation capture | `crates/otter-jit/src/artifact/relocation.rs:1-26,50-60,108-165` | Symbolic `RelocationTarget`s. Records only when capture is enabled (:224-233); `render` (:263-288) builds `relocations.json` and `code-normalized.bin`. |
| Stub address table | `crates/otter-jit/src/entry.rs:66-126` (`TransitionTable`) | Absolute runtime-stub addresses, resolved once per compiler hook. |
| Code object | `crates/otter-jit/src/code.rs:21-24` | One `dynasmrt::ExecutableBuffer` per compiled function (W^X mapping). |
| Code registry and entry cells | `otter-vm/src/jit_registry.rs:1-44`, `native_abi/code_entry.rs:1-80` (`FunctionEntryCell` :41-51) | Stable per-function cells point at a per-generation `CodeEntryCell`. |
| `otter build` | `crates/otter-cli/src/main.rs:726-756` has no Build variant; `README.md:64` | Does not exist. |
| `otter-vm-codegen` | `crates/otter-vm-codegen/Cargo.toml` | Only builds the `gen-pow10` table generator. It has nothing to do with code artifacts. |

## 2. Data layouts (byte sizes)

**Values and handles**
- `Value` is 8 B. A heap cell stores the *full* address `cage_base | offset`; the low 32 bits are the `Gc` offset and the high half is the cage prefix (`value/tag.rs:13-31`, `value/mod.rs:363-372`).
  - The module doc at `value/mod.rs:35-36` says "bits 32..48 stay zero". That is stale.
- `Gc<T>` is a 4 B cage offset (`compressed.rs:181-189`). `GcHeader` is 8 B (`header.rs:14-18`).
- The cage is 4 GiB-aligned and allocated through `std::alloc`, so its base is decided by the allocator and ASLR (`compressed.rs:57-70,506-520`). There is one cage per process (:80).
  - `DEFAULT_CAGE_SIZE_BYTES` is 2 GiB, but its doc says 256 MiB (:46-52).
- A page is 256 KiB (`page.rs:60`). `PageHeader` holds space, flags, `bump_cursor`, `allocated_bytes`/`live_bytes` (usize), `survival_age`, `cage_offset`, `span_pages` and a card bitmap (`page.rs:138-166`).

**Bytecode, three forms of each instruction**
- Wire form: 22 B per instruction (op u8, count u8, 4×u32 inline, u32 overflow) (`binary.rs:446-468`).
- `otter_bytecode::Instruction`: 24 B (`wordcode.rs:38-46`).
- `CodeBlockInstruction`: 32 B (`executable.rs:1347-1368`).

**Closures**
- `ClosureCallHeader` (`closure.rs:97-108`): fid u32, flags u32, `upvalue_base` u64 (an absolute process address), count u32, `eval_env` u32 = 24 B.
- `upvalue_base` always equals `captures_ptr()` (body plus a fixed offset) and is rewritten on every trace (`closure.rs:238-241,404-410`).

**Objects**
- `ObjectBody`: slab handle at header+24, inline values at header+64 (`template/arm64/values.rs:334-337`).
- It also caches an absolute `values_ptr: Cell<*mut Value>` (`object.rs:746`).

**Native functions**
- `NativeFunctionBody` stores `native_ref: u32` (an external-ref index) *and* a raw Rust fn pointer in `NativeCallSlot::Static` (`native_function.rs:187-194,217-230,444-458`).

**Entry cells**
- `FunctionEntryCell` is 16 B, align 8: `generation_cell` AtomicU64, fid u32, params u16, regs u16 (`code_entry.rs:41-51`).

**Normalized code** (`relocation.rs:40-60,937-980`)
- Header: `OTJNCODE` (8 B), arch u16, item count u32.
- Items: RAW tag 0 plus 4 B; RELOC tag 1 plus reg, width, target; BRANCH tag 2 plus kind plus a u32 ordinal; x86 RAW_BYTES tag 3.
- Target ids: 1 stub, 2 cage base, 3 property source cell, 4 template operand slice, 6 guarded heap ref, 8 direct-call entry cell, 10 global lexical cell, 11 deopt data, 12 string constant cell, 13 lookup table, 14 transition table.

## 3. Hot paths

### H1. Bootstrap compile-cache hit (per bootstrap script, every launch)
1. `cache_key` hashes the whole source text with BLAKE3 (`compile_cache.rs:259-277`).
2. File access (`unix.rs:49-56,100-111,136-144,186-196`): `open`+`fstat` the root, `openat`+`fstat` the prefix directory, then `fstatat`+`openat`+`fstat`+`metadata` on the file. That is about 8 syscalls before reading.
3. Reads go through an 8 KiB stack chunk into a `Vec` (`compile_cache.rs:306-319`).
4. `decode_module` allocates `Vec`s and `String`s, then runs the full verifier (`binary.rs`, `verifier.rs`).
5. `rebase_to` works in place (`verifier.rs:164-190`).
6. `build_chunk` converts 24 B records to 32 B records and builds the UTF-8 `AtomTable` (`code_space.rs:369-374`). `resolve_atoms` interns every string constant (`property_atom.rs:286-291`).

Each cached byte is therefore copied at least three times: kernel→chunk→Vec, Vec→decoded module, module→CodeBlock. The bootstrap JS **still executes** to build the heap; the cache only skips parse and compile.

### H2. Snapshot restore (in-process only; `restore.rs:66-263`)
1. `restore_old_space`:
   - Build a `HashSet` of captured page offsets.
   - Allocate pages, holding back any whose offsets overlap the captured set, then copy the bytes (`heap_image.rs:340-379`).
   - Three full object walks: sever (:393-408), relocate through the trace table (:425-455), and a re-trace so bodies recompute cached absolute pointers (:466-479).
2. Relocate the fixed roots.
3. `bump_next_shape_id_to` (`restore.rs:93`).
4. Rebuild the external-ref table **with the raw addresses** (:94-96).
5. Replay atoms (:98-106).
6. Construct an `Interpreter` literal with every cache empty (:109-263).

### H3. JIT address baking
- `TransitionTable::entry` is an O(1) lookup (`entry.rs:104-114`).
- Every address goes into `emit_load_u64`: `movz` plus up to 3 `movk`, variable length (`template/arm64/values.rs:45-56`). x86 uses `mov r64, imm64` (10 B, `relocation.rs:326-334`).
- Relocation records are pushed only when `artifact_request.is_some()` (`template/arm64.rs:161`). Installed code carries **no relocation table**.
- No ADR, ADRP or literal loads are emitted; the normalizer rejects them (`relocation.rs:1153-1166`; grep finds no uses). Code is position-independent apart from these absolute immediates.

### H4. Generated-code sequences that embed process-local data (read from the code; counts are estimates)
- **Runtime-stub call** (`template/arm64.rs:1697-1716`): `mov x0,x20`, then 2–4 `movz/movk` for x16, `blr x16`, then 2 compare/branch pairs on the status.
  - Estimate: 3 instructions for x16, because macOS arm64 image addresses usually have bits 48–63 zero.
- **Object-header load** (`ic_probe.rs:222-247`): cell test (2 instructions to build the mask, then `tst`/`b.ne`), `mov w12,w9`, 2 instructions for the cage base, `add`, then `ldrb`/`cmp`/`b.ne`. About 12 instructions.
  - Because `Value` already holds `cage_base|offset` (`tag.rs:24-31`), the `mov`/cage/`add` (4 instructions) rebuild the address that is already in x9.
  - `direct_call.rs:966-968` dereferences the Value directly (`ldrb w10,[x9]`).
- **Template monomorphic own-slot property load** (`properties.rs:84-104`): about 39 instructions and about 6 loads.
  - Header ≈12, lookup-state guard ≈7, `emit_check_shape` ≈12, slab base plus load ≈6, store plus branch 2.
  - `emit_load_header` already runs `emit_ordinary_lookup_state_guard` (`ic_probe.rs:226-227`), and `emit_check_shape` runs it again (`ic_probe.rs:268`). That is 6–7 duplicated instructions per shape guard.
  - The shape handle is a **raw immediate** (`ic_probe.rs:287`).
- **Property IC miss**: materializes the `PropertySourceCell` address, which holds only `(fid, pc)` (`entry/runtime_ops/vm_ops.rs:37-46`), plus a stub address (`properties.rs:109-127`).
- **Direct call linkage** (`arm64/direct_call.rs:1101-1160`): cell address (2–4 instructions), `add`, `ldar`, `cbnz`, `str`, `ldr` frame bytes, `cbz`, `subs`/`b.lo` stack check, `ldr`/`cmp`/`b.lo` limit, `ldr x16,[x25]`, `cbz`, 2 header loads and about 4 stores. About 25 instructions before argument copy, with three dependent loads (function cell → generation cell → entry).
  - Target identity is guarded by a raw `box_function_id(fid)` immediate (:804-808, 956-960).
- **Machine tier megamorphic probe**: the atom id and its hash are raw immediates (`machine/numeric/arm64/megamorphic_property.rs:70,92`).

Raw `emit_load_u64(` call sites: 449. Recorded symbol/stub call sites: 132 (rg counts across both architectures).

## 4. Invariants relied on
- **Cage:** single, 4 GiB-aligned, below 2^48. Tracers rewrite **only the low 4 bytes** of a Value slot (`value/mod.rs:1922-1941`), so the high half must equal the current process cage prefix. A heap image therefore cannot move to another cage base without an extra pass.
- **Shapes:** pinned in old space and immortal, which is why shape offsets can be code immediates (`shape_body.rs:25-26`, `object.rs:3956-3958`).
- **Shape ids:** `ShapeId` comes from a process-global counter (`object.rs:123-142`). Restore must bump it.
- **Atom ids:** assigned in isolate intern order and baked into shapes and code (`snapshot.rs:16-18,39-40`).
- **Function ids:** global per code space and rebased at link (`code_space.rs:44-47`). They also appear inside heap Values (`Value::function_id`, `tag.rs:21`).
- **Heap image:** requires an empty nursery and no large-space objects (`heap_image.rs:262-266`). Relocation stays idempotent because captured and restored offset sets are kept disjoint (:333-339). Foreign fields are severed before tracing (:388-392). Cached absolute pointers are recomputed by the re-trace (:461-465).
- **Snapshot:** the code space is shared by `Arc` (`snapshot.rs:67-70`). Static native entries are valid only in the same process (`restore.rs:15-21`). A fixed-root count mismatch panics (:22-24).
- **Compile cache:** every hit is verified (`compile_cache.rs:27-28`). Entries are keyed by the build fingerprint rather than a format version (:16-20, `binary.rs:29-31`).
- **JIT:**
  - Entry cells are never reused and callers bake only stable cell addresses (`code_entry.rs:14-26`, `jit_registry.rs:37-40`).
  - No movable pointer lives only in a machine register across a safepoint (`entry.rs:27-29`). Header pointers are valid only until the next safepoint (`ic_probe.rs:210-212`).
  - Global-lexical cells are non-moving (`jit.rs:1107-1111`). String-constant cells are stable Rust allocations (`jit.rs:1114-1120`).
  - `DeoptRuntime` must live exactly as long as its code (`deopt.rs:662-666`).
  - Capture never changes the emitted bytes (`artifact.rs:19-22`).

## 5. Duplication and collapsible transitions
1. **Three instruction formats plus both kept in memory.** Wire (22 B), `Instruction` (24 B) and `CodeBlockInstruction` (32 B). `ChunkPayload` retains both `module` and `executable` (`code_space.rs:82-89`).
2. **Each string constant stored four times:** UTF-16 in `Constant::String` (`binary.rs:366-369`), UTF-8 in `AtomTable` (`property_atom.rs:233-236`), the `NameInterner` entry (:289), and a per-isolate string-constant cell for the JIT.
3. **Four copies of `emit_load_u64`:** `template/arm64/values.rs:45`, `arm64/direct_call.rs:253`, `machine/numeric/arm64.rs:5271`, `template/x86_64.rs:3581`. There are also two symbolic wrappers (`values.rs:60-70`, `direct_call.rs:425-434`).
4. **Two ways to get an object header from a Value:** decompress through the cage (`ic_probe.rs:231-246`) or dereference the Value directly (`direct_call.rs:966`).
5. **Lookup-state guard emitted twice** on the load-header plus shape-guard path (`ic_probe.rs:226,268`).
6. **Native entry stored twice:** raw fn pointer and `native_ref` index (`native_function.rs:444-458`).
7. **Derivable absolute pointers stored in bodies:** `upvalue_base` (`closure.rs:404-410`) and `values_ptr` (`object.rs:746`). Both need recompute-on-trace and restore fixups.
8. **`JitCompileSnapshot.cage_base`** (`jit.rs:280-284`) copies the process global `CAGE_BASE` (`compressed.rs:80`) into every emitted address sequence.
9. **`PropertySourceCell`:** an address used only to carry `(fid, pc)`, which the published frame already provides (`properties.rs:108-110`, `vm_ops.rs:37-46`).
10. **Two function-id→code maps:** the interpreter's `jit_code`/`jit_optimized_code` plus single-entry caches (`lib.rs:1252-1269`), and the registry's `FunctionEntryCell`s.
11. **Two Interpreter constructors** listing every field: `interp/init.rs` (e.g. :338) and `interp/restore.rs:109-263`.
12. **Two restore layers:** `IsolateSnapshot` carries side records (regexp payloads, array flags) outside the page image (`snapshot.rs:98-103`).

## 6. What would have to change

**(a) A much cheaper hot path**
- **Pinned cage-base register.** Load it once in the prologue from `JitCtx`. This removes 2 instructions at every decompress site and removes the most common relocation target. (V8's arm64 pinned cage register is external knowledge, not from this repo.)
- **Stop decompressing Value cells.** x9 is already the header address (`tag.rs:24-31`). Saves about 4 instructions per header load.
- **Call stubs through a table.** `ldr x16,[x20,#slot]; blr` instead of 3–4 `movz/movk`. This is also the form a portable artifact needs.
- **Delete the duplicated lookup-state guard** in `emit_check_shape` when the header guard has already run.
- **Replace `PropertySourceCell` addresses** with immediates or the frame PC.
- **Derive `upvalue_base` from the closure pointer.** Saves 8 B per closure and removes the write inside every trace (relevant to the Earley profile, where closures are allocated in old space).
- **Collapse the bytecode representations:** decode the wire format straight into `CodeBlockInstruction` and drop the retained `BytecodeModule` wordcode.

**(b) A portable AOT artifact** (no process-local addresses)
- **Code**
  - Make relocation recording always on, and use a fixed-width materialization form or table-relative loads.
  - Add a loader that resolves `RelocationTarget` → address. The symbolic vocabulary already exists and ADR/ADRP are already excluded.
  - Add relocation kinds for the non-address isolate-local immediates: shape offsets (`ic_probe.rs:287`), function ids (`direct_call.rs:804`), atom ids and atom hashes (`megamorphic_property.rs:70,92`).
  - Serialize `DeoptRuntime`, safepoints and CacheIR plans. Today only JSON diagnostics exist.
  - Add a `JitFunctionCode` implementation backed by bytes in `__TEXT` instead of an `ExecutableBuffer` (`code.rs:21`), registered through `jit_registry`.
  - Compiles depend on runtime feedback (`jit.rs:289-324`). AOT needs either feedback-free lowering or trained feedback whose shapes live in the image at fixed offsets.
- **Heap**
  - Give the image a byte format (none exists: `heap_image.rs:39-41`).
  - Make restore preserve cage offsets. It currently re-places pages (`heap_image.rs:11-16`).
  - Either map the cage at a fixed virtual address or add a pass that rewrites the prefix of every Value.
  - Build the native table at build time instead of in install order (`external_refs.rs:9-12`), and stop storing raw fn pointers in bodies.
  - Close the three non-self-contained body types.
  - Persist the shape-id floor, the atom table and the function-id bases.
- **Bytecode**
  - The flat format is already free of addresses, since ids are rebased at link (`code_space.rs:790-843`), so it can be embedded as-is.
  - The cache would need to cover the user module graph, which it does not today.
- **CLI**
  - A new `build` subcommand plus packaging. Constraint from general knowledge, not the repo: under macOS hardened runtime, `__TEXT` cannot be patched, so data must be reached through registers or tables rather than baked immediates.

## 7. Open questions
- Relocation capture has no completeness check. It validates only the ranges it recorded (`relocation.rs:263-288`). Of the 449 raw `emit_load_u64` sites, I could not prove that none materializes an address without recording it.
- Does old-space compaction ever move `ShapeBody`? The docs say pinned, but I did not find the enforcement mechanism.
- Actual stub-address instruction widths depend on ASLR layout at runtime; not measured.
- Target ids 5, 7 and 9 are unused. They may be retired kinds; I could not confirm.
- Can the bootstrap produce large-space objects? Capture rejects them (`heap_image.rs:263`).
- Whether the documented in-process restore speedup (a prior-session note, not in the repo) still holds; not re-measured (read-only task).
- `JitCompileSnapshot.cage_base == 0` disables inline access (`jit.rs:283-284`). I did not trace when that happens.