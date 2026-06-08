# Research Prompt — Clean-room RegExp engine (`otter-regex`)

> Hand this file to a coding agent that is an expert in regular-expression
> engine implementation and the ECMAScript RegExp specification. It is a
> **research + design** brief. Do **not** start writing the production engine
> until the research deliverables below are produced and reviewed.

---

## 0. Mission

Otter (a JavaScript engine in Rust) currently depends on a **fork** of the
third-party crate `regress` (`crates/otter-vm` and `crates/otter-compiler`,
pinned at `git = "https://github.com/octoshikari/regress"`, fork source on disk
at `../regress`). We are cutting that dependency and building our **own
best-in-class ECMAScript RegExp engine** as a first-party crate
(`crates/otter-regex`).

Ambition level: **not a regress clone — a better engine.** The goal is a
correctness- and performance-leading ECMAScript regex implementation that we
fully own. We are not constrained to the fork's design choices; where the
research finds a stronger approach, we take it.

Two reasons we own it:

1. **Licensing / maintenance / independence.** Upstream is `MIT OR Apache-2.0`
   today, but we do not control future relicensing, upstream declines
   AI-authored PRs (so we cannot upstream our spec fixes), and we want zero
   dependence on that maintainer. We will not be tied to their roadmap or their
   bugs.
2. **Conformance + perf control.** We close ECMAScript spec gaps and tune
   performance on our own schedule, measured against Test262.

### Design stance (read carefully)

- **Standalone and VM-agnostic.** `otter-regex` must be a self-contained engine
  with **zero dependency on the VM, GC, or any Otter runtime crate.** It
  operates purely on input slices (`&[u16]` / `&[u8]`) and integer ranges. The
  VM later *consumes* this finished, independent engine — the engine never reaches
  back into the VM. It should be conceptually liftable out of the repo as its own
  thing, even though **we will not publish it** (in-repo crate, `publish = false`).
- **Clean-room reimplementation, not a copy.** You may *read* `../regress` to
  understand its approach and the API shape Otter consumes. You may **not** copy
  its source, comments, module structure, identifiers, or unicode-table
  generation verbatim. Reimplement from the ECMAScript specification, the
  algorithm literature, and your own engineering. Treat `../regress` as one
  reference among several (§2.5), not the blueprint. When in doubt, cite the
  spec, not the fork.

---

## 1. What Otter actually uses (the API contract you must satisfy)

Keep the rewrite scoped to what the engine consumes. The full surface in use:

```
regress::Regex
regress::Regex::with_flags
regress::Flags          (::default)
regress::ExecConfig
regress::Match
```

- Compile from a UTF-16 / `&[u16]` pattern (the `utf16` feature is enabled in
  `Cargo.toml`). Otter strings are UTF-16-oriented — see the VM string model.
- `Regex::with_flags(pattern, flags)` returns a compiled regex or a parse error.
- Execute against a UTF-16 subject with a configurable **start offset** (sticky
  `y` / `lastIndex`) via something equivalent to `ExecConfig`.
- A `Match` exposes: overall range, per-group ranges (`Option<Range>` for
  unmatched groups), and named-group access.

**Deliverable check:** read these two files and enumerate every method/field of
the above types that Otter touches, with file:line. The new crate's public API
must cover exactly this set (you may design a *better* API, but then you also
own the adapter edits):

- `crates/otter-vm/src/regexp.rs` (591 lines) — the `Regex` wrapper.
- `crates/otter-vm/src/regexp_prototype.rs` (1912 lines) — `RegExp.prototype.*`
  (`exec`, `test`, `@@match`, `@@matchAll`, `@@replace`, `@@search`, `@@split`,
  `Symbol.species`, flag getters, `d`/`hasIndices`).
- `crates/otter-compiler/src/expr/literal.rs` — regex *literal* validation at
  parse time.

Do not break this contract. If you change the API, the same PR must update every
call site and keep `cargo build -p otter-vm -p otter-compiler` green.

---

## 2. Reference architecture to study in `../regress` (understand, don't copy)

Map each stage, write down *what it does* and *why*, then decide whether to keep
the shape or improve it:

| Stage | File (ref) | Concern |
|-------|-----------|---------|
| Parse pattern → AST/IR | `parse.rs` (2101), `ir.rs` (596) | spec §22.2.1 grammar, flags, early errors |
| Optimize IR | `optimizer.rs` (544) | literal folding, char-class merge, anchors |
| Lower to bytecode | `emit.rs` (410), `insn.rs` (217) | instruction set design |
| Backtracking executor | `classicalbacktrack.rs` (1257) | capturing, backrefs, lookaround |
| Linear-time executor | `pikevm.rs` (501) | NFA/Thompson, no exponential blowup |
| Prefilter | `startpredicate.rs` (223), `bytesearch.rs` (387) | fast scan for required prefix |
| Char sets | `codepointset.rs` (504), `charclasses.rs` (81) | code-point ranges, set ops |
| Unicode data | `unicode.rs` (584), `unicodetables.rs` (33975, **generated**) | `\p{...}` property escapes |
| String cursors | `indexing.rs` (1292), `cursor.rs`, `position.rs` | UTF-8 vs UTF-16 traversal |
| Public API | `api.rs` (1146), `exec.rs`, `types.rs` | the surface we replace |

For unicode tables: **do not vendor their generated file.** Research how to
generate equivalent tables from the Unicode Character Database (UCD) ourselves
(crate option: `unicode-*` crates, or a `build.rs`/codegen step reading UCD
data files). Document the chosen approach and licensing of the data source.

### 2.5 Survey the broader field (to beat the fork, not match it)

The fork is **one** data point. To build a best-in-class engine, study how the
strongest regex engines and the algorithm literature solve the hard parts, then
pick the best ideas (adapting, never copying code). Research and write up:

**Execution-model literature**
- Thompson NFA construction and the classic linear-time simulation.
- Pike VM (NFA simulation **with** submatch/capture tracking) — the basis for
  linear-time capturing.
- Backtracking VMs with explicit instruction sets (the
  "Regular Expression Matching: the Virtual Machine Approach" line of work).
- Glushkov / position automata and Brzozowski/Antimirov **derivatives** as
  alternative construction strategies.
- Lazy/online **DFA** construction with bounded state cache (the linear-time,
  RE2-style guarantee) and when a DFA can't be used for ECMAScript features
  (backrefs, lookaround, captures).
- **Hybrid engine selection**: how production engines route a pattern to
  DFA vs PikeVM vs bounded-backtracking based on the features it uses, and how
  they bound worst-case time/space to defeat catastrophic backtracking
  (ReDoS). Document a concrete selection rule for us.

**Performance techniques to evaluate**
- Literal prefiltering / required-substring extraction, multi-pattern scanning,
  and SIMD-accelerated substring search (`memchr`-class techniques, Teddy/
  shift-or style multi-literal scans).
- Start-position prediction and anchored fast paths.
- Bytecode design that's cache-friendly; sparse-set NFA state representation
  (the Briggs & Torczon sparse set used to track visited states in O(1)).
- Case-folding and unicode class compilation strategies that keep `i`+`u` fast.
- UTF-16 vs UTF-8 input traversal trade-offs (Otter feeds UTF-16).

**Reference engines worth dissecting (designs/papers/docs, not code copy)**
- Rust's mature `regex` + `regex-automata` stack (hybrid lazy-DFA + PikeVM +
  backtracking, literal prefilters) — closest existing Rust art for the
  perf/safety bar we want, though it is **not** ECMAScript-semantics.
- A bounded-DFA linear-time engine design (the RE2 approach) for the
  no-catastrophic-backtracking guarantee.
- High-throughput multi-literal scanners (Hyperscan-style) for prefilter ideas.
- The fork (`../regress`) and any other ECMAScript-grammar engines, specifically
  for **spec semantics** (where general-purpose engines diverge from ES rules:
  Annex B, `lastIndex`, empty-match advancement, `v`-flag set algebra).

**Deliverable:** a short comparative write-up — for each technique, *what it
buys us*, *whether it's compatible with full ECMAScript semantics* (many DFA
tricks break on backrefs/lookbehind/captures), and *whether we adopt it now,
later, or never*. This is what justifies our architecture choice in §4.2 and is
the difference between "another regress" and "the engine we actually want."

> Note on attribution: studying these designs is for **ideas and algorithms**.
> Our source stays clean-room — no copied code, and our own public docs/comments
> describe Otter on its own terms (do not name other engines in shipped
> source/docs; keep engine comparisons inside this internal research note).

---

## 3. Spec-gap research (the core of this task)

Produce a **conformance gap matrix**: for each ECMAScript RegExp feature, state
whether the `../regress` fork supports it, whether it is correct, and what it
would take us to implement. Anchor every row to ECMA-262 §22.2 and verify
empirically against Test262 (`test262/test/built-ins/RegExp/**` and
`test262/test/language/literals/regexp/**`).

Features to evaluate (extend as needed):

- **Flags:** `g i m s u y d v`. In particular:
  - `v` flag (**unicodeSets**, §22.2.1.x) — set notation `[a&&[^b]]`, nested
    classes, `\q{...}` string alternatives, properties of strings
    `\p{Basic_Emoji}`. *Suspected major gap in the fork — confirm.*
  - `d` flag (`hasIndices`) — match-index reporting and `indices.groups`.
- **Unicode mode (`u`):** surrogate handling, `\u{...}`, code-point-aware
  quantifiers, character class ranges, case-folding (`i`+`u` Simple Case Folding).
- **Property escapes** `\p{...}` / `\P{...}`: General_Category, Script,
  Script_Extensions, binary properties; which UCD version, completeness.
- **Named groups** `(?<name>...)`, **backrefs** `\k<name>` and numeric `\1`,
  including forward refs and duplicate names across alternatives (ES2025).
- **Lookbehind** `(?<=...)` / `(?<!...)`, variable-length, with capture.
- **Lookahead**, atomic-ish behavior, quantifier greediness/laziness.
- **Annex B** (sloppy/legacy): `\d`-style in non-`u`, octal escapes, lone `]`,
  legacy `{`, identity escapes, quantifiable assertions.
- **Edge correctness:** empty-match advancement in global loops, `lastIndex`
  semantics under `g`/`y`, zero-width assertions, `$`/`^` with `m`,
  `.` with/without `s`.
- **Performance / DoS:** catastrophic backtracking. When does the fork pick
  PikeVM vs backtracking? Can we guarantee no exponential blowup, or do we need
  a step/time budget? Note: Otter's Test262 harness has heap/timeout guards —
  align with those.

For each gap, classify: **Missing** / **Buggy** / **Correct**, with a Test262
test id or a minimal repro (`Regex::with_flags(...)` + expected vs actual).

---

## 4. Research deliverables (produce these BEFORE coding)

Write them into `docs/` (e.g. `docs/regex-rewrite-research.md`). Required:

1. **API contract sheet** — every `regress::*` symbol Otter uses, mapped to the
   new crate's planned API (§1), with the file:line call sites.
2. **Architecture decision** — backtracking only, PikeVM only, or hybrid (and
   the selection rule). Justify against the conformance + DoS-safety + perf
   trade-off. Note that Otter subjects are UTF-16.
3. **Conformance gap matrix** (§3) with current fork baseline measured by
   running the relevant Test262 dirs through Otter *today*, so we have a
   before/after number. Use the repo's runner:
   `just test262-dir "built-ins/RegExp"` and
   `just test262-dir "language/literals/regexp"`.
4. **Unicode data plan** — how `\p{...}` tables get generated/sourced without
   copying the fork's generated file; UCD version; build-time vs vendored;
   licensing.
5. **Crate skeleton** — proposed `crates/otter-regex/` module layout, public
   types, feature flags (need `utf16`; probably no `no_std` requirement —
   confirm against Otter's build), and `Cargo.toml` with **`publish = false`**
   (in-repo only, never published) and a **dependency allowlist that contains
   zero Otter crates** — no `otter-vm`, `otter-gc`, `otter-runtime`,
   `otter-compiler`. The dependency edge is one-way: VM → `otter-regex`, never
   the reverse. **Normal third-party Rust crates are fine** (e.g. a
   `memchr`-class scanner, UCD/`unicode-*` helpers, `bitflags`) — pick
   well-maintained ones and note why; the only hard ban is depending on any
   Otter crate. The crate must build and test in full isolation
   (`cargo test -p otter-regex` with the rest of the workspace irrelevant).
6. **Migration plan** — phased: (a) stand up crate + parser + one executor
   passing a subset; (b) swap Otter's `regress` import behind the new crate for
   a feature-gated A/B; (c) reach parity on Test262; (d) delete the `regress`
   dependency from `Cargo.toml` workspace + the two crate `Cargo.toml`s.
7. **Risk list** — where clean-room is hardest (unicode tables, case folding,
   `v`-flag set algebra, backref+lookbehind interaction) and the fallback if a
   feature slips.

---

## 5. Implementation rules (for when coding is approved)

These come from the repo's `CLAUDE.md` / `AGENTS.md` and are non-negotiable:

- **Code is written for an LLM reader.** Every Rust module gets accurate
  top-level `//!` docs in the active-engine style: **purpose**, `# Contents`,
  `# Invariants`, `# See also`. Cite the ECMA-262 section a module implements.
- **Minimum inline comments, maximum production-ready code.** No comments that
  restate adjacent code — only *why* comments survive. Self-documenting names.
- **Spec-exact, no hacks.** Build faithful to ECMA-262 §22.2. No shortcuts that
  pass a test but violate semantics. Fix root causes; never skip with an excuse.
- **Never name other engines** in any doc or comment. Describe Otter on its own
  terms. (This includes not crediting the fork inside our source — clean room.)
- **Deterministic collections** where output order matters (`IndexMap`/`BTreeMap`,
  not `HashMap`) — e.g. named-group ordering.
- **Recursion safety** — the parser must have explicit depth limits (pathological
  nested groups/classes); prefer an explicit stack where feasible.
- **No regex-to-parse-JS.** (Obvious here, but: this crate parses *regex*
  patterns, using its own grammar, not `oxc`.)
- **GC / unsafe:** the regex engine is a leaf library — keep it `unsafe`-free if
  practical; if not, justify each `unsafe` and keep it isolated. It must not
  depend on `otter-gc`/VM internals (operate on `&[u16]` + ranges).

## 6. Verification strategy (how "done" is measured)

- **Unit tests** in `crates/otter-regex/` covering each spec feature in §3, with
  spec-section references in test names.
- **Integration through the VM:** the real proof is Otter's RegExp behavior.
  Run, before and after, and report deltas (the repo's TDD workflow — measure,
  fix, re-measure):
  ```bash
  just test262-dir "built-ins/RegExp"        2>&1 | tail -5
  just test262-dir "language/literals/regexp" 2>&1 | tail -5
  just test262-filter "RegExp/prototype"      2>&1 | tail -5
  cargo test -p otter-vm regexp
  ```
- **No regressions** elsewhere: `cargo test --all` stays green and the overall
  Test262 pass-rate must not drop (current baseline ~90.86%; see
  `ES_CONFORMANCE.md` / the dashboard). Report a pass-rate delta for the RegExp
  dirs specifically.
- **Perf sanity:** a backtracking-bomb pattern must not hang the harness (respect
  the existing heap/timeout guards).

---

## 7. First actions for the agent

1. Read `crates/otter-vm/src/regexp.rs`, `regexp_prototype.rs`, and
   `crates/otter-compiler/src/expr/literal.rs`. Produce the §4.1 API contract.
2. Skim `../regress/src/*` per the §2 table to understand the reference design
   (understand only — no copying).
3. Run the §6 Test262 RegExp dirs through Otter *as-is* to capture the baseline
   numbers for the gap matrix.
4. Draft `docs/regex-rewrite-research.md` with deliverables §4.1–§4.7.
5. Stop and request review of the research doc before writing the engine.

**Do not begin the production rewrite until the research doc is reviewed.**
