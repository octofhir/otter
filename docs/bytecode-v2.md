# Otter bytecode v2

## Why

Bytecode v1 stores executable functions as `Box<[ExecInstr]>`. Each
`ExecInstr` is a fixed-size struct holding opcode, operand count, three
inline operands, a side-operand offset for variadic ops, and a property
IC site id. The program counter is an **instruction index** into that
boxed slice.

This shape blocks every downstream layer that wants byte-level
addressing: a baseline JIT (Sparkplug-style) needs PC = byte offset to
patch in safepoints; an inspector breakpoint needs PC = byte offset to
identify an instruction stably across runs; the source map needs PC =
byte offset to make `pc → (line, col)` cheap.

v2 freezes a versioned, variable-length, byte-stream encoding for every
opcode. The PC becomes a **byte offset** into the function's `code:
Box<[u8]>` buffer. The compiler emits bytes; the dispatch loop reads
opcode + operands by stepping a `pc: u32` byte cursor.

## Goals

1. PC is byte offset. `frame.pc: u32` semantics flip with no layout
   change.
2. One opcode schema declares operand layout per opcode in one place.
   Encoder, decoder, disassembler, and JIT generators all read it.
3. Variable-length encoding so a single-operand op like `LoadInt8` is
   2 bytes, not 32.
4. Side operands (long arg lists for `Call`, register slices for
   `CollectArguments`) inline into the stream as a length-prefixed
   tail — no separate side-operand table.
5. Per-function header carries: register/local/scratch counts, flags
   (strict / arrow / rest / async / generator / arguments-kind), source
   map handle, IC slot count, byte-offset upper bound.
6. Source map is a sorted `(pc, span)` table indexed by `pc`.
7. Module-level magic + schema version. Any reader that opens a stream
   with a mismatched version stops cleanly.

## Stream encoding

```
function-code := byte*
byte          := opcode | operand-byte
opcode        := u8
operand-byte* := per opcode-schema entry, in declaration order
```

### Operand types

| Type | Width | Notes |
|---|---|---|
| `Register` | u16 | Little-endian. |
| `ConstIndex` | u32 | Little-endian. |
| `Imm8` | i8 | Sign-extended on decode. |
| `Imm32` | i32 | Little-endian. |
| `Offset` | i32 | Signed branch displacement, byte offset relative to instruction-end pc. |
| `IcSite` | u32 | Dense IC slot id; module-local. |
| `VariadicRegisters` | u8 length + `Register`-typed entries | First byte is `count`, then `count` `Register` values. |

The decoder is generated from the schema; there is no per-opcode hand
decoder.

### Schema

```rust
pub struct OpSchema {
    pub op: Op,
    pub operands: &'static [OperandKind],
    pub has_ic_site: bool,
}

pub enum OperandKind {
    Register,
    ConstIndex,
    Imm8,
    Imm32,
    Offset,
    IcSite,
    VariadicRegisters,
}

pub static OPCODE_SCHEMA: [OpSchema; OP_COUNT] = [ … ];
```

The encoder walks the schema, writes opcode byte + operand bytes in
order. The decoder walks the schema in reverse to step `pc` past one
instruction without decoding operands when the dispatcher only wants
the next pc (used by tracing and unwind).

`OPCODE_SCHEMA` is the **only** place that knows the operand layout
for an opcode. Adding a new opcode means adding one schema row;
encoder, decoder, and disassembler pick it up automatically.

## PC semantics

```rust
pub struct Frame {
    function_id: u32,
    pc: u32,          // BYTE offset into function.code
    …
}
```

- `frame.pc = 0` at entry.
- After dispatching one instruction, the loop does `frame.pc +=
  instruction_len(op, &code[frame.pc..])`. `instruction_len` is the
  schema-driven byte stride; it does not decode operands.
- `Op::Jump(off: Offset)` sets `frame.pc = (frame.pc + 1 + off) as
  u32` (post-opcode-byte base, like x86 relative jumps), so a jump
  target is always a valid opcode-byte address by construction.
- Exception unwind, tracing, and breakpoints all key off byte-offset
  PC. Stable across compiler reruns of the same source.

## Source map

```rust
pub struct SourceMap {
    pub entries: Box<[SpanEntry]>,
}

pub struct SpanEntry {
    pub pc: u32,         // byte offset
    pub span: (u32, u32),
}
```

`entries` is sorted by `pc`. `SourceMap::span_for(pc)` does a binary
search. Same shape as v1's `BytecodeFunction::spans`; only the `pc`
semantics change from instruction-index to byte-offset.

## Per-function header

```rust
pub struct FunctionV2 {
    pub id: u32,
    pub name: Box<str>,
    pub module_url: Box<str>,

    pub param_count: u16,
    pub register_count: u16,   // params + locals + scratch
    pub own_upvalue_count: u16,
    pub ic_site_count: u32,

    pub flags: FunctionFlagsV2,
    pub arguments_object_kind: ArgumentsObjectKind,
    pub mapped_argument_bindings: Box<[MappedArgumentBinding]>,

    pub code: Box<[u8]>,        // the byte stream
    pub source_map: SourceMap,
}

pub struct FunctionFlagsV2(u16);
// bits: strict, arrow, has_rest, async, generator, async_generator,
//       needs_arguments, is_module.
```

`FunctionFlagsV2` packs the eight v1 booleans into a u16 so the hot
path reads them as one word.

## Module-level header

```rust
pub struct BytecodeModuleV2 {
    pub schema_version: u16,
    pub functions: Box<[FunctionV2]>,
    pub constants: Box<[ConstantValue]>,
    pub …
}

pub const BYTECODE_SCHEMA_VERSION: u16 = 2;
```

Disasm + dump (`--dump-bytecode[=json]`) read `schema_version` before
the function list; mismatch produces a clean error, not a panic.

## Compiler emission

Today the compiler builds `Vec<Instruction>` then freezes into
`Box<[ExecInstr]>` via `ExecutableModule::from_bytecode`.

v2 path:

1. Compiler keeps emitting `Instruction` records during code
   generation — the high-level shape (opcode + typed operands) is
   still convenient to manipulate while inserting jumps.
2. A new `BytecodeWriter` consumes the `Instruction` stream, resolves
   jump labels, and emits the byte stream. Forward jumps are
   back-patched once the target pc is known.
3. The frozen output is `BytecodeModuleV2`.

The `Instruction` DTO stays for the compiler-internal IR; only the
final frozen shape changes.

## Dispatch

The current dispatch loop fetches `function.code[frame.pc as usize]`
to get `ExecInstr`, switches on `instr.op`, and reads operands by
index. v2:

```rust
loop {
    let pc = frame.pc as usize;
    let op = Op::from_byte(code[pc])?;
    // Each arm reads its operand bytes off `&code[pc + 1..]` using
    // the schema-typed accessor (no allocation).
    match op {
        Op::LoadInt8 => {
            let dst = read_u16(&code[pc + 1..]);
            let imm = read_i8(&code[pc + 3..]);
            frame.registers[dst as usize] = Value::small_int(imm as i32);
            frame.pc += 4;
        }
        // …
    }
}
```

Hot opcodes inline the reads. Cold opcodes go through
`OpSchema::instruction_len(op, &code[pc..])` + a generic operand
walker.

## Variadic operand encoding

`Op::Call` style opcodes that take a variable register list:

```
opcode | dst:Register | callee:Register | argc:u8 | arg0:Register | arg1:Register | ...
```

`argc` is the byte after the fixed operands. The decoder reads
`fixed_size + 1 + argc * sizeof(Register)` bytes for the whole
instruction.

## Versioning

- `BYTECODE_SCHEMA_VERSION: u16 = 2`.
- Schema changes that add an operand to an existing opcode, change
  operand types, or remove an opcode bump the version.
- Adding a brand-new opcode at the next free byte does **not** bump
  the version — readers reject only opcodes they don't recognise, so
  forward-compatibility is opcode-set, not version-tag.
- The CLI `--dump-bytecode` / `--dump-bytecode=json` output includes
  `schema_version` at the top level.

## Migration plan

This is a hard cut-over per the project rule (no long-lived feature
flags in `main`).

1. **Schema + encoder + decoder + roundtrip tests.** Land the
   `OPCODE_SCHEMA` table, the `BytecodeWriter`, the byte-stream
   decoder helpers, and DTO → byte-stream → DTO roundtrip tests for
   every opcode. No dispatcher change yet — v2 lives behind unit
   tests.
2. **Disassembler.** Port `disasm::disassemble` to the byte stream;
   add `--dump-bytecode=v2` flag if the existing JSON shape needs to
   change, else keep stable.
3. **Source map.** Convert `BytecodeFunction::spans` to byte-offset
   PCs as the writer emits.
4. **Executable layout.** Replace `ExecutableFunction::code:
   Box<[ExecInstr]>` + `ExecutableModule::side_operands` with the
   byte stream. `ExecInstr` and `OperandList` delete.
5. **Dispatcher.** Switch the dispatch loop to byte-stream decode.
   Inline reads for the ~10 hottest opcodes; schema-driven decode for
   the rest.
6. **Frame.pc semantics flip** from instruction-index to byte-offset.
   The frame layout doesn't change (`pc: u32`); only the value the
   compiler / jumps / unwinder / source map all agree on changes.
7. **Delete v1.** `Instruction` DTO stays for compiler IR, but
   `ExecInstr`, `OperandList`, and the side-operand table go away.

Each step ships green: `otter-vm --lib`, `otter-runtime --lib`,
clippy, and the Test262 baseline must stay equal to the pre-migration
numbers between steps.

## Scope notes

- v2 ships **byte stream + byte-offset PC** as one cut-over. It does
  not introduce new opcodes, change operand semantics, or remove
  shortcut opcodes (those are separate plan entries).
- Polymorphic IC slots and JIT stack maps build on top of v2 but are
  not in this task.
- Source-map encoding upgrade (line table compression, varint deltas)
  can come later; v2 keeps the existing flat `(pc, span)` shape.
