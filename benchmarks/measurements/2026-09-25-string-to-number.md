# String-to-Number coercion without allocation

Investigation and closing validation: September 25, 2026.

This is a local Apple M1/macOS AArch64 release investigation. The engine base
is the signed commit named in the environment file. Development observations
are not a published engine baseline.

## Starting point

A profile of `mixed-relational` (a relational site whose right operand is the
String `"8"`) showed that every tier spent most of its time in
`ToNumber(string)`, not in the comparison. `to_numeric_kind` and every other
caller widened the heap string into a fresh Rust `String` (`to_lossy_string`).
`to_number_from_string` then lower-cased that text into a second `String`,
only to reject the `inf` / `nan` spellings that Rust's `f64` parser accepts.
Each coercion paid for two heap allocations and two frees.

## Change

- `number::to_number_from_js_string` parses an all-ASCII Latin-1 heap string
  in place, through a `&str` view of its bytes. Every other string, including
  a Latin-1 body with a non-ASCII white-space character such as NBSP and
  every UTF-16 body, keeps the single widening copy. That preserves the
  `StrWhiteSpace` handling.
- `to_number_from_string` compares the sign-stripped literal against
  `infinity`, `inf` and `nan` with `eq_ignore_ascii_case` instead of building
  a lower-cased copy.
- The call sites switch to the new function: abstract `ToNumeric` and loose
  equality, `ToNumber`, typed-array and DataView number conversion, BigInt
  number coercion and the Math argument coercion.

## Final paired result

Three directly alternating pairs per kernel and tier, eight warmups and twenty
samples, pair 2 reversed. The before binary is the previous slice's gate build;
the after binary is this slice's closing-gate build. All 27 processes validate
their checksums. Load average was about 2.8; no builds, tests or agents ran.

| Kernel / tier | Pair 1 before → after, ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| mixed-relational / Interpreter | 39.590 → 29.498 | 37.040 → 29.773 | 39.511 → 29.493 | **−23.54%** | **−20.49%** | −23.46% |
| mixed-relational / Template | 21.935 → 15.156 | 21.930 → 15.121 | 22.051 → 15.177 | **−31.04%** | **−35.78%** | −30.59% |
| mixed-relational / Production | 20.390 → 11.727 | 20.194 → 11.717 | 20.173 → 11.715 | **−42.13%** | **−39.50%** | −41.34% |
| polluted-feedback (control), three tiers | | | | −0.48% to +0.02% | ±0.02% | |
| string-concat (control) / Interpreter, Production | | | | +0.27%, −0.12% | +0.01% | |
| string-concat (control) / Template | 19.344 → 19.311 | 19.488 → 20.586 | 19.271 → 20.663 | +4.18% | +0.01% | +4.46% |

The control kernels perform no string coercion and retire identical
instructions. The Template `string-concat` wall change is bimodal: pair 1 is
flat, and pairs 2 and 3 move by the same amount at identical instruction
counts. It is code-placement noise, and no claim is made. Committed-call
counts are unchanged, 5,599,968 per 28 invocations on the JIT tiers. The
saving comes entirely from the coercion kernel.

At **29 hand-written production Rust lines added** (20 removed, excluding
comments and tests), the ProductionTiered wall gain is **1.45 percentage points
per added line**.

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger.
- VM unit tests for `number` (93/93) and `string` (114/114) pass. The new
  `heap_string_parse_matches_the_text_parser_for_every_body` test compares
  heap and text parsing, bit for bit, for decimal, signed-zero, hex, binary,
  exponent, empty, `Infinity`, case-folded `inf`/`nan`, invalid,
  NBSP-padded Latin-1 and em-space-padded UTF-16 inputs.
- A focused runtime set of 9 binaries covering generic operators, coercion,
  Math guards, boxed arithmetic, the Template differential, typed-array/DataView
  construction and fill order, and arithmetic exit repair passes 37/37 on
  AArch64 and under Rosetta at `OTTER_GC_STRESS` unset, 16, 4 and 1.
- Targeted Test262 on AArch64 and Rosetta with zero failures, crashes,
  timeouts or OOMs in 22 sections. These are the 16 sections recorded by the
  previous slice, each with identical totals and failing sets, plus
  TypedArrayConstructors 715/739, DataView 548/561, isNaN 15/15,
  isFinite 15/15, `expressions/equals` 48/48 and `expressions/unary-plus`
  17/17. Skipped tests are feature-gated.
- `git diff --check` passed.

## Reproduction

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/mixed-relational.js \
  --function engineKernel --expected 4787500 \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-string-to-number-environment.json),
[run counters](2026-09-25-string-to-number-runs.csv) and
[wall samples](2026-09-25-string-to-number-samples.csv) are tracked here.
Binaries, logs and Test262 records are retained under ignored
`benchmarks/results/tonumber-2026-09-25/`.
