# Bytecode v2 cut-over — WIP status

Tracks the in-flight executable-layer cut-over from
`Box<[ExecInstr]>` + `side_operands` to byte-stream + byte-offset PC.
Lives on branch `task-2.1-step3-executable-cutover`; **do not** merge
to `main` until every step below is green.

## Done

- Two-pass jump fixup in `encode_function` translates v1
  instruction-index branch deltas (and `EnterTry` handler offsets)
  into v2 byte-offset deltas relative to `(jump_pc + 1)`.
  `NO_HANDLER_OFFSET` sentinel passes through untouched.
  `crates/otter-bytecode/src/bytecode_v2.rs` + tests.
- `translate_spans_to_byte_pcs` helper rewrites a v1 `[SpanEntry]`
  slice into byte-offset `SpanEntry`s using `instr_to_byte_pc`.
  Out-of-range pc clamps to end-of-stream, matching the encoder's
  "jump past last instruction" convention.

Verified after each commit:

- `cargo test -p otter-vm --lib` — 539/539.
- `cargo test -p otter-runtime --lib` — 123/123.
- `cargo test -p otter-bytecode --lib bytecode_v2` — 19/19.
- `cargo clippy -p otter-bytecode --all-targets --all-features -- -D warnings` — clean.

## Remaining work (in order)

### 1. ExecutableFunction storage flip

Replace `code: Box<[ExecInstr]>` with `code: Box<[u8]>` produced by
`bytecode_v2::encode_function`. Drop `ExecutableModule::side_operands`
(no longer needed once operands live inline in the byte stream).

Add per-`ExecutableFunction`:

- `byte_spans: Box<[SpanEntry]>` — translated via
  `translate_spans_to_byte_pcs` so `error_ops::snapshot_frames` can
  look up source spans by byte-offset PC.
- `property_ic_sites: FxHashMap<u32, u32>` — dense IC site id assigned
  during build for `LoadProperty` / `StoreProperty` / `HasProperty`
  byte offsets. (Or `Box<[(u32, u32)]>` sorted by byte_pc + binary
  search — pick by callsite count.)

### 2. ExecInstr → transient borrow over byte slice

Change `ExecInstr` from owned-record-in-`Box<[ExecInstr]>` to a
short-lived borrow returned by `ExecutableFunction::decode_at(byte_pc)`:

```rust
pub(crate) struct ExecInstr<'a> {
    op: Op,
    operand_len: u8,
    byte_len: u8,
    property_ic_site: u32,
    operand_bytes: &'a [u8], // starts at operand 0 after header
}
```

Operand accessors (`register(idx)`, `const_index(idx)`, `imm32(idx)`,
`operand(idx)`, `operands()`) walk `operand_bytes` per the v2 tag-byte
layout. For sequential access most dispatch arms touch operands 0..N
once, so a tiny iterator cache is fine.

### 3. Dispatch loop fetch + PC advance

In `crates/otter-vm/src/lib.rs` (`dispatch_loop_inner`, ~line 2620):

- Before each iteration: `let instr = function.decode_at(frame.pc)?;`
- After non-branching dispatch: `frame.pc += instr.byte_len();` (was
  `frame.pc += 1`).
- Branch ops continue to call `apply_branch(frame, offset, ...)`; the
  formula `next_pc = (pc + 1) + offset` is unchanged because `offset`
  is now a byte delta and `pc + 1` is the byte after the opcode byte.

Audit every `frame.pc.checked_add(1)` / `frame.pc += 1` site in
`call_ops.rs`, `static_call_ops.rs`, and any other place where a
helper advances PC. Each becomes `+= instr.byte_len` (helpers may need
the byte_len threaded through as a `u32` arg, or fetch the current
instruction's byte_len from the function).

### 4. ExecutionContext.exec_* API surface

API stays signature-compatible. Each `exec_register`,
`exec_const_index`, `exec_imm32`, `exec_operand` forwards to the
borrow's method. `exec_operands` returns a materialized `&[Operand]`
backed by a per-iteration scratch buffer (held by `Interpreter`) so
variadic call sites (`Op::Call`, `Op::NewArray`, …) keep their
existing `&[Operand]` consumers without allocating per dispatch.

### 5. Delete ExecInstr storage path + side_operands

Final cleanup once all callsites compile and tests pass:

- Remove `ExecInstr::from_operands`, `ExecutableModuleBuilder::push_function`'s
  side-table plumbing, `ExecutableModule::side_operands` field.
- `ExecInstr` becomes the transient borrow only.

### 6. Verification

- `cargo test -p otter-vm --lib` ≥ 539.
- `cargo test -p otter-runtime --lib` 123.
- `cargo test -p otter-bytecode --lib bytecode_v2` 19+.
- `cargo clippy --all-targets --all-features -- -D warnings` clean.
- `bash scripts/test262-safe.sh built-ins/Function language/expressions/await language/statements/try language/statements/generators`
  no regression vs `main` baseline.

## Notes / gotchas

- 183 existing callsites use `context.exec_register(instr, idx)` etc.
  Keeping `ExecInstr`'s method shape means most callsites compile
  unchanged once the lifetime is added.
- `error_ops::snapshot_frames` does `partition_point` over
  `fun.spans`. Either move that lookup onto `ExecutableFunction.byte_spans`
  or keep using `BytecodeModule::Function::spans` while remembering
  that `frame.pc` is now byte-offset and the source `spans` are still
  instruction-index — these must match, so the cut-over MUST flip
  spans to byte-offset at the same commit as the dispatch flip.
- `Frame::pc: u32` layout is unchanged. Only the value's meaning
  changes (instruction-index → byte-offset). No public API change.
- `BYTECODE_SCHEMA_VERSION` stays at `2`. The wire format the
  executable builder consumes is the same one already tested by
  `encode_decode_function_roundtrip`.
- Branch-class fixup is already correct for v2. Dispatcher does not
  need to know the encoder's two-pass logic; it sees a finished byte
  stream with byte-offset deltas.

## Don't touch on this branch

- `main` and `origin/main`. Branch holds 28 commits ahead of
  `origin/main` (26 inherited + 2 added on this branch). Do not push
  until the cut-over is fully green.
- `otter-nodejs`, `otter-node-compat` (parked compatibility shims).
