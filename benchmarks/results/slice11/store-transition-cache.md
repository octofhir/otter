# Shared store-transition cache: fixed-work instructions

Measured on macOS ARM64 with the release CLI using `/usr/bin/time -l target/release/otter run <script>`. Each script performs fixed work; the reported metric is `instructions retired`.

| Script | Before this change | With generated transition lookup | Change |
| --- | ---: | ---: | ---: |
| `ts-fixed.js` | 215,243,835,799 | 205,905,309,247 | −9,338,526,552 (−4.34%) |

The before value above was measured in this working tree immediately before the patch. Additional post-change measurements, with the same command, are:

| Script | Instructions retired |
| --- | ---: |
| `zlib-fixed.js` | 400,310,567,886 |
| `crypto-fixed.js` | 20,581,865,804 |
| `../calls/ast_ctor.js` | 33,214,123,509 |

The task brief lists earlier, rounded figures of 411.7G, 21.1G, and 32.3G for these three scripts. They are useful context but were not measured immediately before this patch. The AST constructor workload does not show a gain from this change.

Validation: `cargo run --release -q -p otter-difftest` passed 56/56 on a complete rerun; `cargo test --release -q -p otter-jit --lib` passed 281/281.
