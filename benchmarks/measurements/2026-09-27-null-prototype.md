# Null-prototype objects: representation and creation

Investigation requested while the arguments replacement was being validated.
No object-model change is made in this note. Measurements use the final arguments replacement, including combined
argument-count/cache initialization.

## V8 primary sources

Current [runtime-literals.cc](https://github.com/v8/v8/blob/main/src/runtime/runtime-literals.cc),
`CreateObjectLiteralWithNullProto`, chooses
`slow_object_with_null_prototype_map` and populates dictionary properties.
Local Node 24.16.0 with `%HasFastProperties` reports true for `{x:1}` and false
for both `{__proto__:null,x:1}` and `Object.assign(Object.create(null),{x:1})`.
This establishes representation, not creation latency.

The claim that every read necessarily performs a hash lookup is too broad.
Current [accessor-assembler.cc](https://github.com/v8/v8/blob/main/src/ic/accessor-assembler.cc),
`HandleLoadICSmiHandlerLoadNamedCase`, has a `kNormal` handler with an encoded
dictionary index. It validates capacity/key and reads the matching entry;
failed index validation enters dictionary lookup. This is a dictionary IC,
although it is not the fast field handler used by a shape with fixed fields.
The quoted 500–1500 ns versus 30 ns has not been reproduced here.

## Otter evidence

`object::set_prototype_value` represents null in `jit_proto` without an exotic
sidecar or forced shape removal. `Object.create(null)` uses ordinary object
allocation and this same prototype operation.

A diagnostic fixture alternates null-prototype literals and `Object.create`
objects, each with x/y data properties. The exact optimized `readNull` code map
has one `machinePropertyShapeProof` and two `machinePropertySlotLoad` regions.
Thus these reads already use shape-proven slots. Artifacts are under
`benchmarks/results/parity-2026-09-27/null-prototype-artifacts/`; the event report
is `null-prototype-events.json` and the source is `null-prototype-read.js`.

Creation has a separate limitation: `nullLiteral` falls back to Template because
its `DefineDataProperty` is outside Machine HIR. `nullCreate` compiles into
Machine. This is a concrete compiler/allocation boundary to measure, rather than
an argument for replacing Otter's existing shape representation wholesale.

Construction and access fixtures live under
`benchmarks/fixtures/null-prototype/`. Creation retains 100,000 objects and consumes their
properties, preventing dead-allocation elimination. Access performs two million
reads over 32 distinct receivers. Both compare ordinary literals, null-prototype
literals and `Object.create(null)`. These were run sequentially after compiler/test work finished. Their counts
are separate from the seven agreed parity workloads.

## Fixed-work comparison

`/usr/bin/time -l`, identical stdout, all processes exit 0. These are whole-process
counts including startup, compilation and GC; they do not establish nanoseconds
per allocation. The read fixture also includes indexed access to its receiver
array. Otter uses shape/slot reads for each receiver property, as verified above.

| Fixture | Otter instructions | Node/V8 instructions | Bun/JSC instructions | Otter RSS bytes |
|---|---:|---:|---:|---:|
| create-create.js | 1,277,759,556 | 497,187,495 | 213,201,882 | 70,139,904 |
| create-literal.js | 1,659,048,887 | 621,296,241 | 171,530,014 | 76,496,896 |
| create-ordinary.js | 1,093,434,949 | 341,049,717 | 139,257,205 | 70,139,904 |
| read-create.js | 1,114,698,515 | 686,660,939 | 117,126,630 | 59,523,072 |
| read-literal.js | 1,109,063,177 | 715,916,750 | 118,643,674 | 59,441,152 |
| read-ordinary.js | 1,109,295,335 | 317,628,494 | 116,701,880 | 59,195,392 |

In this workload Otter null-literal creation retires 51.7% more instructions
than ordinary-literal creation; null Object.create is 16.9% higher. Null receiver
reads have no comparable penalty. The measured target is literal construction
and property definition, not null-prototype storage. Raw commands, source and
executable hashes, engine versions and counters are in
`benchmarks/results/parity-2026-09-27/null-prototype-final-counters/`.
Reproduce with `python3 scripts/dev/null-prototype.py <fresh-output-directory>`
on macOS with the release CLI, Node and Bun installed.
