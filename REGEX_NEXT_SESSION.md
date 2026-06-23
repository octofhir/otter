# Regex engine — next-session prompt (goal: beat Node.js)

Paste this whole file as the opening prompt of a fresh session.

---

## Mission

Make `otter-regex` competitive with — and ideally faster than — Node.js (V8
Irregexp) on `benchmarks/scripts/regex.js`, **without** regressing RegExp/String
Test262 and **without** simplified/toy algorithms. Production-grade only.

## Where things stand (do not redo)

Branch `perf/tier1-close` already landed (all Test262-verified, 0 regressions):

- `839e3f77` fold ASCII case into non-Unicode `/i` classes at lowering → fast
  bitmap path.
- `d2688b34` prefilter through leading zero-width assertions (`^`/`$`/`\b`).
- `d4c13967` native `@@match` fast path (global match collects all matches in one
  `engine::find` pass, slices substrings; skips per-match exec protocol). match
  phase 1190ms → 4ms.
- `81f2bd90` native `@@replace` fast path (global, non-sticky, native exec, no
  expando, `$`-free literal template). replace phase 184ms → 2ms.
- `dfeba4f9` fixed the long-standing `OTTER_GC_STRESS` startup crash — **GC
  stress is now a usable verification tool** (`OTTER_GC_STRESS=64/128`).

Result: `regex.js` 1.63s → ~0.36s (was 27x Node, now ~1.5x). The host-side
per-match machinery is no longer the bottleneck.

## The remaining bottleneck — measure it first

Per-operation split (`/tmp/rxsplit.js` style: time `match`/`replace`/`exec`
separately): the residual is **`exec`** (~186ms), and a fresh `samply` profile of
an `exec` loop shows **~87% in the matcher itself** (`Matcher::run` ~64%,
`Matches::next` ~24%) — NOT result-object building.

Root cause: backtracking re-scan. For `/([a-z.]+)@([a-z.]+)/gi`, at every
candidate start the greedy `[a-z.]+` matches the whole letter run, then the
required `@` fails, and the matcher gives back the run one char at a time — all
futile, because `@ ∉ [a-z.]`, so no given-back position can be `@`. This is
O(run length) wasted work per start position, repeated across the whole string
between matches.

## Research first (then decide)

Before writing code, research and write up (short notes file) how the production
engines handle exactly this, and which technique fits otter's
bytecode-backtracker:

1. **V8 Irregexp**: regex compiled to native code / bytecode; how it does
   "global" `RegExp.prototype.exec` loops, and especially its **prefilter /
   required-substring** ("boyer-moore lookahead", `RegExpMacroAssembler`,
   `kAtStart`, the "filter" built from the pattern's required characters). How it
   avoids the futile-give-back re-scan.
2. **JSC YARR**: the YARR interpreter + YARR JIT; its `m_pattern` "term"
   analysis, character-class disjointness, and the "byte-disjunction" / quick
   checks it uses to skip positions.
3. **RE2 / Rust `regex`**: the multi-engine strategy — literal prefilters
   (memchr / Aho-Corasick / Teddy), the **lazy DFA** for finding match bounds,
   the **PikeVM** for captures, and the **one-pass NFA**. Crucially: when each is
   chosen, and why a PikeVM alone is *slower* than backtracking on simple linear
   patterns (per-thread capture clones).

Deliver a short `REGEX_RESEARCH.md` summarizing the above and the chosen plan.

## Candidate techniques, ranked (validate with research)

1. **Required-literal prefilter** (likely best ROI, contained). Extract a literal
   (or literal set) that *every* match must contain — here `@` in
   `([a-z.]+)@(...)`. Then:
   - If the literal is absent in `text[lastIndex..]`, there is **no match** —
     return null without scanning (memchr). Safe, easy, general.
   - Better: use the literal's position to anchor attempts (V8/RE2 style),
     turning O(n·run) into ~O(n).
2. **Possessive-repeat / no-backtrack analysis** (contained, matcher peephole).
   At lowering, when a greedy `C+`/`C*` repeat is immediately followed (through
   zero-width insns only, unconditionally — no `Split`) by a required atom `A`
   with `C ∩ A = ∅`, mark the repeat possessive: skip the give-back loop, since
   no give-back can ever satisfy `A`. CORRECTNESS-CRITICAL: only when provably
   disjoint and unconditional, else you drop valid matches. Kills the
   `[a-z.]+@` futile give-backs directly.
3. **Thompson NFA / PikeVM path** for backref-free, lookaround-free programs.
   O(n·states) single pass, no backtracking blowup. NOTE from prior analysis:
   PikeVM will NOT beat the backtracker on the *simple linear* bench patterns
   (it adds per-thread capture-clone overhead) — it only wins on
   re-scan/ambiguous patterns. Needs an **unfused lowering** (the current
   `Insn::CharSeq`/`Insn::Repeat` consume multiple/variable units and break the
   one-transition-per-position model; add a `fuse: bool` to `ir::lower` to emit
   per-unit `Char` + split loops). Gate on no `BackRef`/`Look`. Share the
   matching primitives (`decode`/`char_eq`/`class_member`/asserts) with the
   backtracker (extract to a shared module) to avoid conformance drift.
4. **Lazy DFA** for match-bounds search + PikeVM/backtracker for captures
   (RE2 architecture). Biggest win, biggest effort — likely only justified if
   1–3 don't close the gap.

Suggested order: do (1) and (2) first (contained, high ROI, low risk), re-measure;
only build (3)/(4) if still behind Node.

## Key files

- `crates/otter-regex/src/program.rs` — `Insn` set (NFA-shaped: Char/Class/
  Split/Jump/Save/asserts/Match + fused CharSeq/Repeat), `Prefilter`.
- `crates/otter-regex/src/ir.rs` — AST→program lowering, `compute_first_set`
  (the existing prefilter builder), fusion (`flush_char_run`, `fuseable_atom`).
- `crates/otter-regex/src/exec/backtrack.rs` — the matcher (`run`, `attempt`,
  `class_member`, `decode`, `char_eq`, asserts). The give-back logic is
  `Resume::RepeatGreedy`.
- `crates/otter-regex/src/api.rs` — `Matches` iterator (leftmost scan + prefilter
  dispatch), `build_match`.
- `crates/otter-vm/src/regexp.rs` — `engine::find` (collects all matches via the
  api iterator), `find_from_utf16`, `REGEX_BACKTRACK_BUDGET`.
- `crates/otter-vm/src/regexp_prototype.rs` — `@@match`/`@@replace`/`exec`
  host glue + the native fast paths (model new ones on these, incl. the
  `is_static_native(proto_exec)` + guard-order + expando/sticky gating).

## Verification protocol (non-negotiable)

- `cargo build --release -p otter-cli`
- `cargo test -p otter-regex --release` and `-p otter-vm --release`
- **Test262 differential**: capture failing set, stash change, rebuild baseline,
  run `just test262-filter "RegExp"` and `"String/prototype"`, diff failing sets.
  Property-escape timeouts are flaky (machine load) — ignore those; require **0
  new non-property-escape failures**. (Harness pattern is in the git history of
  this branch's commit messages.)
- `OTTER_GC_STRESS=64` on touched paths (now works).
- Bench parity: `OTTER_JIT=0` vs `=1` byte-identical output on all
  `benchmarks/scripts/*.js`.

## Constraints

- No simplified algorithms, no feature flags / env kill-switches, no
  benchmark-fitting. Production-grade, single default path.
- Preserve all RegExp semantics: captures, `lastIndex`, sticky, Unicode (`u`/`v`
  surrogate handling), case folding, named groups, `$`-substitution.
- Memory file [[regex_engine_perf]] and [[tier1_close_progress]] have prior
  context.
