# Generic binary operators in the optimizing tier

Investigation and closing validation: September 25, 2026.

This is a local Apple M1/macOS AArch64 release investigation. The engine base
is the signed commit named in the environment file. Development observations
are not a published engine baseline. Kernel hashes are in the environment file.

## Starting point

`jit_machine_guarded_numeric` had been failing on main. Tracing it showed that
Machine HIR declined a **whole function** once any `+ - * / % **` or
`< <= > >=` site observed a non-Number operand ("non-numeric arithmetic
feedback"). Two paths lead to such feedback:

- Ordinary code. A compare that once sees `undefined`, or `"k" + i`, keeps
  its function on the Template tier for good.
- The optimizing tier's own exit repair. After an object operand made a
  compare exit once, the widened feedback made every rebuild decline.

Two related defects came to light along the way:

- **Parameter typing.** Any non-empty feedback on these operators typed their
  parameter operands as Number. A generic site therefore planted a Number
  entry guard, and every String argument exited at entry.
- **Concat exit loop.** A `+` site that had ever seen a String was always
  rebuilt as the `TaggedStringConcat` node. That node exits on a Number pair,
  so it exited again after every recompile.

## Change

1. **Committed generic operator.** In the VM, `ObjectProtocolValueOp` gains
   `Binary(BinaryOperator)`. It completes through the interpreter's own
   kernels (`add_value`, `numeric_binary_value`, `compare_value`) behind the
   existing `jit_object_protocol_value` stub. In the JIT, `semantics.rs`
   classifies a binary site as committed when its feedback is non-empty and
   not Number-only.
   - Primitive-String `+` keeps its concat node until that node exits with
     `typeMismatch`.
   - Committed sites never deoptimize.
2. **Number fast path.** Every generic operator except `%` and `**` gets a
   `machineBinaryNumberProbe`, expanded by `committed_probe` into a probe,
   a cold call and an SSA join.
   - Two Int32 operands complete in general registers. Overflow, a zero
     product and division go to the double path.
   - Other Number pairs complete in IEEE doubles and are boxed through the
     ordinary `BoxNumber` canonicalization.
   - Any other operand misses to the committed call once.
   - AArch64 uses reserved scratch (x15–x17, v30/v31). x86-64 declares
     `TargetClobberSet::NumberProbe` = {xmm14}.
3. **Parameter typing.** Only Number-only feedback types an operand parameter
   as Number.
4. **Root frame.** The binary kernels root their operands in their own handle
   scope, so the committed entry dispatches them before building the generic
   three-slot root frame. A profile had shown that frame taking a third of
   the stub's time.

The first closed series, taken before the Int32 path existed, is excluded
from the result. It showed that routing Int32 operands through the double
unit and `BoxNumber` cut instructions by 42.9% but raised wall time by 17.8%,
because it lengthened the loop's dependency chain.

## Final paired result

Three directly alternating pairs per kernel and tier, eight warmups and twenty
samples, pair 2 reversed. The before binary is the previous engine slice's gate
build. The after binary is this slice's closing-gate build. All 54 processes
validate their checksums. Load average was about 3.5; no builds, tests or agents
ran.

| Kernel / tier | Pair 1 before → after, ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| polluted-feedback / Production | 2.169 → 2.020 | 2.171 → 2.022 | 2.168 → 2.018 | **−6.88%** | **−48.32%** | −4.69% |
| mixed-relational / Production | 21.873 → 20.270 | 21.855 → 20.165 | 21.892 → 20.994 | **−6.40%** | **−9.42%** | −6.36% |
| boxed-double-property / Production (control) | 3.504 → 3.507 | 3.505 → 3.506 | 3.505 → 3.505 | +0.04% | −0.05% | −0.14% |
| polluted-feedback / Template | 2.178 → 2.172 | 2.172 → 2.172 | 2.175 → 2.172 | −0.12% | −0.06% | −0.70% |
| mixed-relational / Template | 21.852 → 22.035 | 22.007 → 23.295 | 21.813 → 21.867 | +2.28% | +0.01% | +2.09% |
| Interpreter (all three) | | | | +0.02% to +6.38% | ±0.05% | |

Interpreter and Template execute none of the new code; their instruction
counts are unchanged. The mixed-relational Interpreter wall change is bimodal
in the before samples (39.7 ms in pair 1, 36.0 ms in pairs 2 and 3) at an
identical instruction count, so it is placement noise and no claim is made.

| ProductionTiered totals across 28 invocations (before → after) | polluted-feedback | mixed-relational |
| --- | ---: | ---: |
| Optimized entries | 0 → 28 | 0 → 28 |
| Reentrant stub transitions | 54 → 54 | 5,599,968 → 5,599,968 |
| Optimizing deopts | 0 → 0 | 0 → 0 |
| Emitted code bytes | 3,984 → 2,344 | 4,156 → 2,104 |

Before this change, both kernels ran entirely on Template because the
optimizer declined them. In `polluted-feedback`, every Number operand now
finishes inside the probe; only the first iteration of each call, which reads
`undefined`, reaches the committed call. In `mixed-relational`, every
iteration still needs the committed operator for its String operand, but the
loop around it is optimized. Its remaining cost is the VM's string-to-number
conversion, which allocates a Rust `String` per call on every tier.

At **464 hand-written production Rust lines added** (31 removed, excluding
comments, tests, benchmarks and documentation), the ProductionTiered wall gain
per added line is **0.0148 percentage points on polluted-feedback** and
0.0138 on mixed-relational.

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger
  (the two new kernels included).
- Machine suites 174/174 (AArch64) and 169/169 (x86-64).
- Focused runtime matrix of 37 test binaries at `OTTER_GC_STRESS` unset, 16, 4
  and 1: 130/130 on AArch64. Under Rosetta, 122/124: the two failures are
  `jit_private_access` and `jit_super_access`, which select the x86-64
  Template tier only and do not run this code.
- Targeted Test262 on AArch64 and Rosetta with zero failures, crashes,
  timeouts or OOMs in 16 sections: addition, subtraction, multiplication,
  division, modulus, exponentiation, the four relational operators,
  compound-assignment, Number, BigInt, String, Math and `expressions/call`.
  Sections recorded by earlier slices match their totals and failing sets.
- `git diff --check` passed.

New regression `crates/otter-runtime/tests/jit_generic_binary_operators.rs`
(on the old code the optimizer declines these functions, so no probe exists):
- polluted relational and additive sites stay optimized and Number operands
  never leave the probe;
- object operands coerce once, left before right, for all ten operators, and
  a throwing `valueOf` reaches the caller's catch;
- Strings, BigInt, Symbol, NaN, negative zero, Int32 overflow and products
  that leave Int32 produce the interpreter's exact results.

## Reproduction

Build `otter-engine-benchmark` with `cargo build --locked --release
-p otter-benchmark --features engine --bin otter-engine-benchmark`, keep
separate before/after executables, and run each tier serially with
`OTTER_GC_STRESS` unset:

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/<kernel>.js \
  --function engineKernel --expected <checksum> \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-generic-binary-operators-environment.json),
[run counters](2026-09-25-generic-binary-operators-runs.csv) and
[wall samples](2026-09-25-generic-binary-operators-samples.csv) are tracked
here. Binaries, logs, profiles and Test262 records are retained under ignored
`benchmarks/results/generic-binary-2026-09-25/`.
