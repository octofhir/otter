# Regex Engine Research: Prefiltering and Futile-Backtrack Avoidance

Technical reference on how production regex engines (V8 Irregexp, JSC YARR, RE2,
Rust `regex`/`regex-automata`) avoid re-scanning during backtracking and how they
prefilter candidate match positions. Source-level, citation-backed. Intended to
guide `otter-regex` work (Pike-VM path, prefilter, possessification).

## Contents

1. [V8 Irregexp](#1-v8-irregexp)
2. [JSC YARR](#2-jsc-yarr)
3. [RE2 and Rust `regex`](#3-re2-and-rust-regex)
4. [Possessive-repeat conversion via class disjointness](#4-possessive-repeat-conversion-via-class-disjointness)
5. [Synthesis: levers for otter-regex](#5-synthesis-levers-for-otter-regex)

---

## 1. V8 Irregexp

Irregexp is V8's backtracking engine. Pattern is parsed to a `RegExpTree`, lowered
to a graph of `RegExpNode` (`src/regexp/regexp-nodes.h/.cc`), then emitted as either
bytecode (interpreted tier) or native code via a `RegExpMacroAssembler` subclass
(`src/regexp/regexp-macro-assembler.{h,cc}`, per-arch `regexp-macro-assembler-*.cc`).
The interesting optimizations live in `src/regexp/regexp-compiler.{h,cc}`. Two-tier
design (v8.dev/blog/regexp-tier-up): all patterns first compile to bytecode and
interpret (memory-cheap); a per-execution "ticks" counter triggers recompilation to
native code when it hits zero.

Three layered mechanisms: **(A)** per-position quick checks (mask/value test over the
next N chars), **(B)** Boyer-Moore lookahead (multi-position skip table), **(C)** the
greedy-loop / disjointness optimization that converts loop-then-required-atom into
fixed-distance back-counting instead of stack backtracking.

### 1.1 Global `exec` loop: `RegExpGlobalCache`

`RegExp.prototype.exec` and the string methods route through `RegExp::Exec`
(`src/regexp/regexp.cc`). For global (`g`) / sticky (`y`) iteration, V8 uses
**`RegExpGlobalCache`** (declared in `regexp.h`). It amortizes match re-entry by
running the matcher in batches and caching multiple results in one register array.

State:
- `register_array_` / `register_array_size_` — flat capture-register buffer; each
  match uses `registers_per_match_` slots (`2 + 2*captures`). Sized for several
  matches at once.
- `num_matches_` — matches filled by the last invocation.
- `current_match_index_` — cursor into the cached matches.

Driver **`FetchNext()`**: increments `current_match_index_`; when the cached batch is
drained (`current_match_index_ == num_matches_`), it re-invokes the compiled regex
starting at the end of the previous match, refilling the buffer.
**`LastSuccessfulMatch()`** returns the register slice for the latest match.

**Empty-match advancement** prevents infinite loops: after a zero-length match,
**`AdvanceZeroLength()`** (spec `AdvanceStringIndex`) bumps the offset — Unicode
(`u`/`v`) checks for a surrogate pair (`IsLeadSurrogate`/`IsTrailSurrogate`) and
advances by 2, otherwise by 1.

Key point: retrying at successive start positions for a non-anchored pattern is **not**
done one char at a time by the global cache. That scanning is pushed *down* into the
compiled code via the Boyer-Moore skip loop and the preamble's advance-and-retry. The
global cache only re-enters the matcher when its batch is exhausted.

### 1.2 `QuickCheckDetails` — mask/value prefilter

`QuickCheckDetails` (`regexp-compiler.h`) is the cheap multi-char rejector:
- `characters_` — how many chars (1-4, packed in a machine word) this check covers.
- `positions_[]` — `Position { uc32 mask; uc32 value; bool determines_perfectly; }`.
  A char `c` at this offset passes iff `(c & mask) == value`. `determines_perfectly`
  means the test is exact (single concrete char, no false positives), so a pass is a
  guaranteed match.
- `mask_` / `value_` — the **rationalized** combined word: `Rationalize(asc)` packs
  the per-position masks into one 16/32/64-bit word so the whole N-char window is
  tested with a single load + AND + CMP.
- `cannot_match_` — set when constraints are contradictory; the compiler emits an
  unconditional backtrack/skip.

`GetQuickCheckDetails(QuickCheckDetails*, RegExpCompiler*, int characters_filled_in,
bool not_at_start)` is virtual on `RegExpNode`, walking the graph forward filling up
to `characters` positions. `TextNode` computes mask/value per char (with case-fold
expansion under `IgnoreCase`), bailing on `read_backward()` (lookbehind). `ChoiceNode`
**intersects across alternatives** — a position constrains the check only if all
alternatives agree.

Emission is **`EmitQuickCheck(...)`**: preload via `LoadCurrentCharacter`, then masked
compare. `need_mask` elides the AND when the load already guarantees the bits.
`fall_through_on_failure` chooses branch polarity (forward to body on success; on
failure fall through to BM skip / advance loop, or branch to backtrack).

### 1.3 `EatsAtLeast` — skip-distance bound

`EatsAtLeast(int still_to_find, int budget, bool not_at_start)` returns the **minimum**
chars a node + successors must consume. Per-node cache `EatsAtLeastInfo` stores
`from_possibly_start` and `from_not_start` (latter excludes start-anchors, which
otherwise lie about minimum consumption). This minimum: sizes the quick-check window,
bounds Boyer-Moore lookahead distance, and enables one up-front bounds check
(`CheckPosition`) over K chars instead of one per char.

### 1.4 Boyer-Moore lookahead — `BoyerMooreLookahead` / skip table

The position prefilter that makes non-anchored search fast (`regexp-compiler.{h,cc}`):
- **`BoyerMoorePositionInfo`** — per-offset char-set summary (`ContainedInLattice`
  word/non-word lattice in `w_` + a char bitset). Modifiers `Set`, `SetInterval`,
  `SetAll`.
- **`BoyerMooreLookahead`** — array of `length_` (`== EatsAtLeast`) per-offset infos
  in `bitmaps_`; `map_count_()` reports distinct char-maps.

`FillInBMInfo(isolate, offset, budget, BoyerMooreLookahead*, not_at_start)` is the
recursive virtual populating per-offset char sets by walking forward from each offset.

**`EmitSkipInstructions(RegExpMacroAssembler*)`** synthesizes the skip: pick the most
selective offset window; if the char at that offset is not in its possible-set, advance
`current_position` by the computed skip and retry — without entering the backtracker.
Macro-assembler primitives: `CheckBitInTable(table, on_in_table)`,
`SkipUntilBitInTable(...)`, `SkipUntilChar(...)`, `AdvanceCurrentPosition(advance_by)`.
The skip table was compacted from a 128-byte scalar table to a **128-bit nibble table
fitting one SIMD register**, so membership tests vectorize.

`SetGuess` is the choice to commit to the BM skip when the first lookahead offset is
already selective enough.

### 1.5 `kAtStart` and start-of-input handling — the `Trace` class

Start state is tracked statically during emission on **`Trace`** (`regexp-compiler.h`)
via `at_start_` (a `TriBool` `FALSE/TRUE/UNKNOWN`, encoded by `AtStartField`). `Trace`
also carries `cp_offset_` (preload-window advance relative to actual position) and
`quick_check_performed_` (a `QuickCheckDetails` recording what the prior quick check
already proved, so successors skip re-testing). The `not_at_start` boolean threaded
through `EatsAtLeast`/`GetQuickCheckDetails`/`FillInBMInfo` *is* this start state — it
stops `^`/start-anchors and `from_not_start` from being applied where the engine can't
guarantee index 0. BM and multi-char preload are limited under multiline (`IsMultiline`).

### 1.6 Disjointness: avoiding re-scan after a greedy class repeat

Core "no futile re-scan" optimization (author Erik Corry; see his Medium post
"Regexp backtracking in loops, and how we can optimize it away"). Example `/[a-b]*[c-d]/`.
Naive backtracking matches `[a-b]` greedily, then on failing `[c-d]` pops one position
and retries — stepping backward char by char. But the loop class `[a-b]` and the
continuation class `[c-d]` are **disjoint**: no consumed char can ever satisfy the
continuation, so *every* backtrack into the loop body is guaranteed to fail.

Irregexp detects this (using the same per-offset char-set info that feeds BM / quick
checks) and replaces stack backtracking with **fixed-distance back-counting**: store the
loop start once; "backtracking" becomes counting backward a fixed number and exiting the
loop immediately — no per-char re-scan, no stack growth.

Strict preconditions: the loop body must be **fixed-length** (single char, or a literal
like `foo` in `/(?:foo)*bar/`) and its char positions disjoint from the continuation. It
does **not** trigger on variable-length bodies like `/(?:an?)*/` (multiple backtrack
lengths → fixed back-count invalid). Proposed generalization: "naturally possessive"
detection when a quantifier body is non-backtracking and disjoint from the continuation.

### 1.7 Greedy-loop emission, `GreedyLoopState`, atomic/possessive "cut"

**`GreedyLoopState`** (`regexp-compiler.h`) bundles loop labels/traces:
`greedy_loop_label()` (back-edge target) and `counter_backtrack_trace()` (fixed
back-count exit). `ChoiceNode::EmitGreedyLoop` emits the body + a back-edge guarded by
**`assembler->CheckGreedyLoop(greedy_loop_label)`** — checks whether the position
advanced since the last iteration and jumps back if so (prevents zero-progress loops).
Under disjointness the exit is the fixed back-count, not stack pops.

`ChoiceNode::EmitChoices` is the generic alternation emitter: per alt (except last)
push position + backtrack location, emit per-alt quick check, on failure pop and fall to
next. `Node::LimitVersions` / `EmitOutOfLineContinuation` (`compiler->AddWork(node)`)
bound code growth by emitting position-independent versions with jumps.

**Atomic groups `(?>...)` / possessive quantifiers** — discard interior backtrack state
on success via the submatch / cut machinery (`regexp-nodes.h`):
- **`ActionNode`** with `ActionType` values `BEGIN_POSITIVE_SUBMATCH`,
  `BEGIN_NEGATIVE_SUBMATCH`, `POSITIVE_SUBMATCH_SUCCESS`, `STORE_POSITION`,
  `CLEAR_CAPTURES`, `SET_REGISTER_FOR_LOOP`. `BEGIN_*_SUBMATCH` saves the **backtrack
  stack pointer** + position into registers at group entry. On success the engine
  **restores the saved stack pointer**, discarding (cutting) every backtrack frame
  pushed inside the group — that pointer reset *is* the "cut".
- **`NegativeSubmatchSuccess`** (`EndNode` subclass) stores `stack_pointer_register_`,
  `current_position_register_`, `clear_capture_count_`, `clear_capture_start_`; on the
  relevant path it restores SP + position and **clears the capture registers** in
  `[clear_capture_start_, +count)` (speculative captures inside a lookaround/cut must
  not leak). `CLEAR_CAPTURES` is the normal-path `ActionNode` form.
- **`NegativeLookaroundChoiceNode`** (`ChoiceNode` subclass) has `kLookaroundIndex = 0`
  (must fail) / `kContinueIndex = 1`; overrides `try_to_emit_quick_check_for_alternative()`
  to return false for the lookaround alt (it consumes nothing observable).
- **`Guard`** + **`GuardedAlternative`** implement loop-counter cut conditions:
  `Guard { register, Relation (LT|GEQ), value }` checked against a loop counter
  (`SET_REGISTER_FOR_LOOP`/`STORE_POSITION`). Bounds `{n,m}` and prevents re-entering
  rejected iterations.

The saved-stack-pointer register is the practical "cut register": restoring after
success deletes all interior choice points in O(1) — mechanism behind both atomic-group
correctness and futile-backtrack elimination.

### 1.8 Irregexp rejection hierarchy (cheapest-first)

For a non-anchored search a position is processed:
1. **Boyer-Moore skip** (`EmitSkipInstructions`, SIMD nibble table) — advance past
   whole spans where no successful start is possible; distance bounded by `EatsAtLeast`.
2. **Quick check** (`QuickCheckDetails` mask/value) — reject with one load+AND+CMP over
   the next 1-4 chars before any node code; `determines_perfectly` skips redundant
   re-checks.
3. Full backtracking node code; within it the greedy-loop / disjointness / submatch-cut
   machinery converts backtracking into fixed back-counts or O(1) cuts.

`RegExpGlobalCache` sits above all of this, batching matches and bumping start offset
(surrogate-aware `AdvanceZeroLength`) so the compiled matcher (with its BM/quick-check
preamble) is re-entered as rarely as possible.

**Sources:** V8 source `src/regexp/` (regexp-compiler.{h,cc}, regexp-nodes.h,
regexp-macro-assembler.cc, regexp.cc) —
https://chromium.googlesource.com/v8/v8/+/refs/heads/main/src/regexp/ ;
v8.dev/blog/regexp-tier-up ; v8.dev/blog/non-backtracking-regexp ;
Erik Corry, "Regexp backtracking in loops…" —
https://medium.com/@erik_68861/regexp-backtracking-in-loops-and-how-we-can-optimize-it-away-ef3b2590f87e ;
SpiderMonkey Irregexp import — https://hacks.mozilla.org/2020/06/a-new-regexp-engine-in-spidermonkey/

---

## 2. JSC YARR

YARR ("Yet Another Regex Runtime") is JSC's **backtracking** engine (not a DFA — chosen
because JS regexes are irregular and DFA compilation is time-prohibitive; see
JSCRegExpProcessingAndJSCGoals wiki). With no DFA for automatic linear scanning, all
"avoid re-scan" cleverness lives in (a) precomputed pattern metadata, (b) a
Boyer-Moore-style prefilter generated by the JIT, (c) anchoring/BOL rewrites. Source:
`Source/JavaScriptCore/yarr/` — `YarrPattern.{h,cpp}`, `YarrInterpreter.{h,cpp}`,
`YarrJIT.{h,cpp}`; driver `runtime/RegExp.cpp`.

### 2.1 Architecture: interpreter vs JIT

Pipeline: parse → `YarrPattern` (tree of `PatternDisjunction` / `PatternAlternative` /
`PatternTerm`) → either `Yarr::jitCompile` (machine code) or `byteCompilePattern`
(`ByteDisjunction` of `ByteTerm`). Execute via `RegExp::matchInline()`.

Selection (`RegExp.cpp`): JIT is attempted unless
`pattern.containsUnsignedLengthPattern()`, JIT disabled, or features JIT can't emit —
notably **back-references** (`m_containsBackreferences`) and some **lookbehind**
(`m_containsLookbehinds`). Otherwise falls back to `ByteCode` (`byteCodeCompilePattern`).
`m_state` ∈ `{NotCompiled, ByteCode, JITCode, ParseError}`.

Two JIT modes: `JITCompileMode::MatchOnly` (no capture writes — for `test`, `search`)
vs `IncludeSubpatterns`. `YarrCodeBlock` stores `m_ref8`/`m_ref16` (full) and
`m_matchOnly8`/`m_matchOnly16`, each with `InlineStats`, plus an inline fast path for
short subjects.

Bytecode model: a `ByteTerm` is a tagged union (`enum class Type`, ~29 values) with
`atom`, `quantityType`+`quantityMaxCount`, `inputPosition`, `frameLocation`,
`OptionSet<Flags>`, `m_capture`, `m_invert`, `m_matchDirection`. Specialized per
quantifier shape: `PatternCharacterOnce/Fixed/Greedy/NonGreedy`, `CharacterClass`,
`BackReference`, `ParenthesesSubpattern`, `AssertionBOL/EOL/WordBoundary`,
`DotStarEnclosure`, body framing `BodyAlternativeBegin/Disjunction/End`.

### 2.2 `YarrPattern` term analysis + precomputed minimum sizes

`PatternTerm` (`YarrPattern.h`): `Type` ∈ {`AssertionBOL/EOL/WordBoundary`,
`PatternCharacter`, `CharacterClass`, `NumberedBackReference`, `NamedBackReference`,
forward refs, `ParenthesesSubpattern`, `ParentheticalAssertion`, `DotStarEnclosure`}.
Quantifiers `QuantifierType { FixedCount, Greedy, NonGreedy }` + `quantityMinCount` /
`quantityMaxCount`. Flags: `m_capture`, `m_invert`, **`m_possessive`** (no
backtracking), `m_matchDirection`.

`PatternAlternative`: `m_terms`, precomputed **`m_minimumSize`** (min chars consumed),
`m_hasFixedSize`, `m_startsWithBOL`, `m_containsBOL`, subpattern id range, `m_direction`.
`PatternDisjunction`: alternatives + aggregate `m_minimumSize`, `m_hasFixedSize`,
`m_callFrameSize`. `YarrPattern` root: `m_body`, `m_numSubpatterns`, and feature flags
`m_containsBOL`, `m_containsBackreferences`, `m_containsLookbehinds`, `m_containsModifiers`.

Minimum-size computation (`YarrPattern.cpp`): `setupAlternativeOffsets` accumulates
input position;
`alternative->m_minimumSize = currentInputPosition - initialInputPosition`. Non-fixed
quantifier sets `m_hasFixedSize = false`. `setupDisjunctionOffsets` takes the **min over
alternatives**. `m_body->m_minimumSize` is the most important number for skipping start
positions: both backends use it for the bounds check (`checkInput`,
`branch32(BelowOrEqual, index, length)`) so they never attempt a start index with fewer
than `m_minimumSize` chars remaining.

### 2.3 Character classes, canonicalization, disjointness

Built by `CharacterClassConstructor`: `m_matches8`/`m_ranges8` (Latin-1) +
`m_matches32`/`m_ranges32` (non-Latin-1), kept sorted (`addSorted`) for binary-search
membership + set algebra (`Union`/`Intersection`/`Subtraction` over `BitSet<0x100>` /
chunked `BitSet<2048>`). Canonicalization win: `[12345]` becomes a range test
`(1 <= c && c <= 5)`.

**Possessive / disjointness pruning** — `optimizePossessiveQuantifiers()` with helper
`followerForcesPossessive`: if a greedy term's accepted set is provably disjoint from the
mandatory first char of the follower, convert to possessive (`m_possessive`),
eliminating its `BackTrackInfo` and entire backtracking record. The static
"byte-disjunction" optimization. Related: `/.*abc.*/` collapses to `DotStarEnclosure`;
runs of literals are checked up to 8 at once (one wide load+compare).

### 2.4 Anchoring, BOL, candidate start positions

This is the core of "don't try every index."

BOL metadata: during parsing, `assertionBOL()` marks an alternative anchored only when
`^` is genuinely leading (no preceding terms, not inverted, forward direction) → sets
`m_startsWithBOL`, `m_containsBOL`, `m_pattern.m_containsBOL`.

**`optimizeBOL()`**: rewrites `/^a|^b|c/` — copies BOL-anchored alts into a separate
"loop" disjunction (`copyDisjunction(..., filterStartsWithBOL=true)`) and marks
originals **`onceThrough`** (`ByteTerm::alternative.onceThrough`). Effect: a `^`-anchored
alt is attempted only at line-start positions, not every index. Bails for
multiline / modifiers:
`if (m_containsModifiers || !m_containsBOL || multiline()) return;`.

Per-index advance loop (interpreter, in the backtracking path of body framing terms in
`matchDisjunction()`): when all alts fail at index N, do **`input.next()`** (advance by
1) and replay the body — but `pattern->sticky()` short-circuits the whole loop for
`/.../y` and anchored patterns (only one start position). `onceThrough` alts are excluded.

First-character / required-character optimization (JIT): instead of replaying the
automaton at each index, the JIT fast-scans for a position where the leading char
matches, then attempts the rest. `m_checkedOffset` tracks validated input;
`checkInput()` emits `branch32(BelowOrEqual, index, length)`. With `m_hasFixedSize` the
JIT computes start lazily at the end. `firstCharacterAdditionalReadSize` register
handles non-BMP leading codepoints (advance by 2).

### 2.5 Boyer-Moore-Horspool prefilter (main JIT skip-ahead)

For more than a single leading char, the JIT builds a BM-style lookahead bitmap filter
to jump multiple positions at once.

Bitmap (`YarrJIT.h`): `BoyerMooreBitmap` wraps `Map m_map = WTF::BitSet<128>`
(`mapSize=128`, `mapMask=127`), `m_count`, and `BoyerMooreFastCandidates
m_charactersFastPath` (`CharacterVector`, `maxSize=2`). `add()` sets bit
`character & mapMask`; `isAllSet()` (count==128) means unconstrained → optimization
impossible. `YarrBoyerMooreData::m_maps` dedups identical bitmaps across compilations.

Building the lookahead window (`BoyerMooreInfo`, `findBestCharacterSequence`,
`findWorthwhileCharacterSequenceForLookahead`): walk forward collecting, per offset, the
set of chars that can appear there across all alternatives (intersected across branches).
Commit `bb136cc` extended this to nested disjunctions (`/aaa|(bbb|cccc)/`), bailing at
any offset that can match any char (intersection → universal). Reported +12% on
JetStream2/regexp.

Choosing anchor + skip distance: balance sequence length (longer → bigger skip on miss)
against candidate density (fewer chars → stronger filter); iteratively raise the
candidate-per-char limit from 4 up to 32. The classic BMH skip table is built for
patterns <=256 chars. On a non-match at the anchor, the table gives the max safe shift.

**Subject-adaptive anchor selection** (commit `d43e2a2`, `SubjectSampler`): BM is only
fast if the anchor is rare *in the actual subject*. Samples `sampleSize=128` chars from
the **middle** of the subject (`half = (length-128)/2`, to avoid edge bias), builds a
frequency histogram. Decision: `matchingProbability = (mapSize/2) - frequency`, times
sequence length; pick highest score, **reject BM entirely if nothing scores positive**
(cutoff ~50%: a char appearing in >50% of samples makes BM useless). Pick the
less-frequent char as anchor. +5-10% on regex-dna-SP. So YARR picks the anchor *per
execution* based on the data being scanned.

### 2.6 Backtracking records + pruning

`BackTrackInfo` structs (in the `Interpreter` template, `YarrInterpreter.cpp`) — each
quantified/branching term reserves a slot at `term.frameLocation` in `context->frame`:
- `BackTrackInfoPatternCharacter` / `…CharacterClass` / `…BackReference` —
  `{ matchAmount, begin }`.
- `BackTrackInfoAlternative` — branch/return bookkeeping.
- `BackTrackInfoParentheses` — `{ matchAmount; ParenthesesDisjunctionContext*
  lastContext; begin }` — a linked list of `ParenthesesDisjunctionContext` (each with
  `next` + `subpatternBackup[]`) to unwind nested-subpattern iterations.
- plus `…ParenthesesOnce`, `…ParenthesesTerminal`, `…ParentheticalAssertion`.

Greedy match + backtrack: consume up to `quantityMaxCount` recording
`backTrack->matchAmount`; on failure decrement `matchAmount` and replay from saved
`begin`. Saved `matchAmount`/`begin` make a backtrack a cheap pointer reset, not a
re-scan.

Pruning levers: **possessive conversion** (deletes the `BackTrackInfo` slot entirely);
**`onceThrough`** alts excluded from re-loop; **`DotStarEnclosure`**
(`matchDotStarEnclosure`) sets match bounds in O(1) for `/^.*…$/` with `dotAll`;
specialized single-quantity term types emit minimal machinery (no-quantifier / fixed-
count need no backtrack record). Bugzilla 60860 reworked the JIT so backtracking is
structured fall-through/return chains, letting the BM prefilter + required-char scan slot
cleanly in front.

(Caveat: per-struct `BackTrackInfo` field names were paraphrased from WebFetch summaries,
not verbatim quotes; accurate in substance — verify exact declarations in the file.)

**Sources:** WebKit yarr source —
https://github.com/WebKit/WebKit/blob/main/Source/JavaScriptCore/yarr/ (YarrPattern,
YarrInterpreter, YarrJIT .h/.cpp), runtime/RegExp.cpp ;
JSCRegExpProcessingAndJSCGoals wiki — https://trac.webkit.org/wiki/JSCRegExpProcessingAndJSCGoals ;
commits d43e2a2 (SubjectSampler), bb136cc (Extend BoyerMoore), d6426c8 (BMH to DFG/FTL) ;
Bugzilla 60860 (Simplify backtracking in YARR JIT).

---

## 3. RE2 and Rust `regex`

Both use an engine portfolio dispatched by *the question asked* (bounds vs captures) and
the regex/string size. Guiding principle (Gallant): **engines with more functionality
search more slowly**, so keep a portfolio and pick the fastest one that can answer.

### 3.1 Rust `regex-automata` engine portfolio

The crate (>=1.9) is a facade over `regex-automata`, which exposes each engine standalone:

- **`nfa::thompson::pikevm::PikeVM`** — Thompson NFA simulation in lock-step. Handles
  *every* feature (all captures, Unicode `\b`, any length). `O(m·n)` guaranteed. Tool of
  last resort: never fails/panics, so the meta engine can always fall back. Slowest.
- **`nfa::thompson::backtrack::BoundedBacktracker`** — backtracking with a `(state,
  position)` visited bitmap bounding worst case to `O(m·n)` (no catastrophic
  backtracking). Supports captures + `\b`. ~2x faster than PikeVM. **Fails** when
  `len(regex)*len(haystack) > visited_capacity` → small inputs only.
- **`dfa::onepass::DFA`** — one-pass DFA. Reports capture offsets at near-DFA speed, but
  only for the "one-pass" subset (each DFA state ↔ <=1 NFA state). **Requires anchored
  search.** Fixed memory; build fails if exceeded. When applicable, "likely faster than
  any alternative" for captures.
- **`hybrid::dfa::DFA` / `hybrid::regex::Regex`** — lazy DFA (builds states on demand,
  <=1 new state+transition per byte; avoids `O(2^m)` build). Near full-DFA speed.
  **Match span only — no captures.** Cannot do Unicode `\b` on non-ASCII. Needs a
  mutable `Cache`.
- **`dfa::dense::DFA` / `dfa::sparse::DFA`** — fully compiled DFA. Fastest search,
  zero-copy deserialization, but `O(2^m)` build → not for untrusted patterns. Span only.
- **`meta::Regex`** — the meta engine; composes all of the above and **never returns a
  `MatchError` to the caller** ("will attempt a lazy DFA even if it might fail … then
  restart with a slower but more capable engine").

Selection (`meta/strategy.rs`): `Strategy::new()` tries `Pre` (prefilter-only); else
builds `Core`, then tries to wrap with one of `ReverseAnchored` / `ReverseSuffix` /
`ReverseInner` (in order), else bare `Core`.

- **`Pre`** (no regex engine, prefilter only): the regex's whole language is a finite
  small exact literal set — single pattern, no captures, no lookaround, LeftmostFirst
  (e.g. `foo`, `foo|bar`, `foo[1-3]`). Runs only `Prefilter::find()`/`prefix()`.
- **`Core`** (baseline): all engines + optional `Prefilter`. Per search tries, in
  decreasing speed / increasing capability: full DFA → lazy DFA → one-pass (anchored) →
  BoundedBacktracker → PikeVM, falling through (`search_nofail` guarantees an answer via
  PikeVM).
- **`ReverseAnchored`**: anchored at end (`$`) not start, DFAs available → scan
  *backward* from the end (`try_search_half_rev`).
- **`ReverseSuffix`**: unanchored-at-start, DFA available, a longest common *suffix*
  literal extractable, no fast forward prefilter exists → run suffix prefilter forward,
  bounded reverse DFA (`try_search_half_rev_limited`, anti-quadratic guard), then
  forward DFA for span.
- **`ReverseInner`**: LeftmostFirst, unanchored-at-start, DFA available, an *inner*
  required literal (with a prefix) extractable → prefilter the inner literal, reverse-
  scan the prefix, forward-scan the suffix (`try_search_half_fwd_stopat`), with quadratic
  detection.

### 3.2 Literal prefilters

Extraction (`regex_syntax::hir::literal`): `Extractor` walks the `Hir` → a `Seq` of
`Literal`s; `ExtractKind` selects prefix vs suffix. Expands `?` and small classes /
bounded repeats (`(foo|bar|quux)(\s+\w+)` → inexact prefixes `foo, bar, quux`). Each
`Literal` is **exact** (reaches a match state) or **inexact** (prefix of longer matches,
e.g. `bar` from `bar+`). Ordered to preserve leftmost-first (Perl) priority.

Extraction is "one big heuristic": keep the set small; prefer longer/rarer literals
(1-2 byte literals too common → high false positives); a space flags the literal as low
value; infinite/too-large sequences disable the optimization. "Substring searches can be
an order of magnitude faster than a regex search."

Search backends behind `util::prefilter::Prefilter`:
- **Single literal → `memchr::memmem`** — Two-Way (`O(n)`, const space), Rabin-Karp for
  very short needles, and a generic SIMD path that picks the **two rarest needle bytes**
  (byte-frequency table), detects coincidences with vector ops, verifies only at
  candidates.
- **Multiple literals → Teddy** (ported from Hyperscan, via `aho-corasick`) — SIMD
  multi-substring candidate detection, then verification, preserving leftmost-first.
- **Aho-Corasick** — fallback when Teddy isn't usable (very large literal counts). When
  literal-match perf is similar the meta engine often prefers the lazy DFA over A-C
  (better composition).

A prefilter yields candidate offsets; the engine runs only at/near them (or only an
anchored verification) instead of every position.

### 3.3 Split "find bounds" from "resolve captures"

When captures are wanted, the meta engine does two phases:
1. **Bounds (fast):** unanchored lazy/full DFA → only `Match::span` (start/end bytes).
   The DFA ignores captures entirely → fast.
2. **Captures (precise, narrow):** an **anchored** PikeVM / BoundedBacktracker / one-pass
   DFA run **only on the matched span**, never the whole haystack.

"Run the lazy DFA first to find the bounds, and then only run the PikeVM or
BoundedBacktracker to find the capture offsets." Faster because the expensive
capture-tracking engine processes a tiny region; the DFA (no captures, but far faster per
byte) does the linear sweep. Anchoring via `Input::anchored(Anchored::Yes)`; one-pass
*requires* anchored, satisfied here.

### 3.4 Why PikeVM is slower than a backtracker on simple patterns

**PikeVM cost.** Advances *all* live NFA threads in lock-step per byte. With captures
every live thread carries its own **slot array** of capture offsets. At each byte it
computes the epsilon-closure over all active states, and when threads split (alternation,
`*`) it must **copy the slot array** so each path keeps independent capture state —
copy-on-step. That is `O(len(regex))` slot copies *per byte* on top of `O(states × text)`
bookkeeping. Dominant constant = the per-thread capture slots cloned on every split.
PikeVM also cannot speculatively follow a single path — it must keep every
leftmost-first-viable path alive simultaneously for correctness in `O(m·n)`.

**Backtracker advantage.** `BoundedBacktracker` explores **one path at a time** with a
**single** slot array mutated as it descends/unwinds — no per-thread slot duplication,
lazy on-demand epsilon closures. The `(state, position)` visited bitmap prevents
re-exploring a pair, capping at `O(m·n)`. Cost: `O(m·n)` *space* for the bitmap (small
inputs only), but ~2x faster than PikeVM in practice by sidestepping slot-cloning and
all-paths-in-parallel overhead.

**One-pass NFA — eliminating thread duplication.** A regex is "one-pass" when at every
byte only one alternative is viable, so the NFA determinizes with <=1 NFA state per DFA
state → at most one live path → **only one slot array, never copied** (Cox: "the one-pass
NFA implementation never needs to make a copy of the submatch boundary set"). Each byte is
a const-time table lookup that also does the single slot update → DFA-class speed *with*
captures. Catch: most unanchored regexes are not one-pass because the implicit
`(?s-u:.)*?` unanchored prefix is itself ambiguous — which is exactly why one-pass is only
used in the anchored capture-resolution phase. One-pass: `x*yx*`, `([^x]*)x(.*)`;
not one-pass: `(.*)x(.*)`.

### 3.5 RE2's structuring

Four engines selected by question + size:
- **DFA** (`dfa.cc`): "does it match / where are the bounds." Lazy states, **cache
  flushes entirely when full** (fixed memory: free all and restart), sparse sets for
  `O(1)` state dedup. No captures. Bounds: run **forward DFA** to find the end, then
  **DFA backward** from that end to find the start (two DFA passes, no NFA).
- **OnePass** (`onepass.cc`): one-pass regexes; never copies the submatch boundary set.
- **BitState** (`bitstate.cc`): bounded backtracking with a `(state, position)` bitmap,
  used only when the bitmap is <=32KB (small regex × small string).
- **NFA** (`nfa.cc`): general capture engine for ambiguous regexes; copies submatch
  boundary sets → slower but linear; invoked only on the located match region.

Prefix accel / required-prefix detection (`prog.cc`, `dfa.cc`): detect a common literal
prefix to accelerate the DFA's "looking for a new match" loop — single-byte → `memchr`
(rare-byte/SIMD trick); length-1-not-shift-DFA → `PrefixAccel_FrontAndBack()`; multi-byte
→ a **shift DFA** via `BuildShiftDFA()`. The loop calls `prog_->PrefixAccel(p, ep-p)`; a
`NULL` result means the prefix is absent in the remainder → jump to the end. Direct
analog of the Rust crate's memmem/Teddy front-end feeding the automaton.

**Cross-engine design decision (both systems):** find match bounds with a capture-free
automaton (DFA/lazy DFA), accelerated by a literal prefilter, then resolve captures with
the cheapest capable engine (one-pass > backtracker > NFA/PikeVM) on the narrowed span
only.

**Sources:** Andrew Gallant, "Regex engine internals as a library" —
https://burntsushi.net/regex-internals/ ; regex-automata docs — https://docs.rs/regex-automata/ ;
meta strategy — https://github.com/rust-lang/regex/blob/master/regex-automata/src/meta/strategy.rs ;
regex-syntax hir::literal — https://docs.rs/regex-syntax/latest/regex_syntax/hir/literal/index.html ;
Russ Cox, "Regular Expression Matching in the Wild" — https://swtch.com/~rsc/regexp/regexp3.html ;
RE2 dfa.cc / prog.cc — https://github.com/google/re2/blob/main/re2/

---

## 4. Possessive-repeat conversion via class disjointness

**In one sentence:** when a greedy `C+` (or `C*`/`C?`/`C{n,m}`) is immediately followed
by a required atom `A` with **C ∩ A = ∅**, drop the give-back/backtracking edge and make
the repeat possessive/atomic (`C++`/`C*+`) — *without changing accepted strings or `C`'s
submatch results*. The canonical anti-ReDoS transform; PCRE2's `pcre2_auto_possess.c` is
the reference, applied automatically at compile time.

### 4.1 Soundness argument

Setup: greedy `C+` consumes the maximal run of `C`-chars at `[i, i+r)` (all in `C`; then
`s_{i+r} ∉ C` or end). A backtracker records give-back choice points: after the maximal
match, on suffix failure it returns one char at a time, re-attempting `A` at `i+r-1,
i+r-2, …, i`.

Claim: if `C ∩ A = ∅`, every give-back attempt is guaranteed to fail at `A`, so give-back
can never produce a match maximal consumption didn't already expose → delete give-back
edges (make possessive).

Proof: give-back tries `A` at some `p ∈ {i, …, i+r-1}` (a position *inside* the run),
having released `s_p … s_{i+r-1}`. For an overall match via that backtrack, `A` must match
`s_p`. But `s_p` was consumed by `C+` → `s_p ∈ C`. Since `C ∩ A = ∅`, `s_p ∉ A`, so `A`
cannot match at `p`. Holds for **every** interior give-back position `p < i+r`. The only
position `A` can succeed is `p = i+r` (where `s_{i+r} ∉ C`, eligible to be in `A`) —
exactly what possessive/atomic tries. The two are observationally equivalent for the whole
pattern, with identical run length `r` (maximal munch in both). ∎

This is Friedl's "introduce failure cheaply" — atomic/possessive lets the engine *commit*
to the maximal run because no give-back can rescue the match. Canonical safe example
`[^"]*+"` (the negated class cannot match the closing quote).

Two subtleties the proof needs:
1. `A` must be **required** (not optional) and the **unique** continuation. The run-final
   position `i+r` is the only one possessification preserves; if the continuation could
   match an interior position (because it can match a `C`-char), correctness breaks.
   Disjointness forbids exactly this.
2. It does not preserve `A`'s submatch slicing against the run — but `A` could never match
   the run chars anyway, so there is nothing to preserve. Captures inside `C+` are
   preserved (run length `r` unchanged).

### 4.2 PCRE2 auto-possessification (`pcre2_auto_possess.c`)

A **post-compilation pass over the bytecode**. Header: "scan a compiled pattern and
change repeats into possessive repeats where possible." Entry function "replaces single
character iterations with their possessive alternatives if appropriate … modifies the
compiled opcode."

Building the base list — `get_chr_property_list`: a compact descriptor of what the
repeated item matches. `list[0]` = normalized opcode (`OP_CHAR`, `OP_DIGIT`, `OP_NOT`,
class); `list[1]` = a can-match-empty/greedy flag; `list[2..]` = char codes, or
type+data pairs for char-types/Unicode props, or a 32-byte class bitset reference. So
`\d+` → `{OP_DIGIT, …}`.

The disjointness test — `compare_opcodes`: "checks whether the base and the current
opcode have a common character, in which case the base cannot be possessified." Compares
the repeated item's base list against the following opcode(s), proving empty intersection
by case:
- **Char vs char:** inequality. Equal → overlap → bail.
- **Class vs class:** AND the two 256-bit bitsets; nonzero → overlap → bail.
- **Char-type vs char-type:** static table `autoposstab[][]` precomputes disjointness
  among `\d \D \s \S \w \W . \R \h \H \v \V \X` (`1` = distinct/safe). `\d` vs `\D`
  distinct; `\d` vs `\w` overlap.
- **Unicode properties:** `propposstab[][]` / `catposstab[][]` for `\p{…}`/`\P{…}`
  general-vs-specific category combos.
- **End of pattern (`OP_END`):** greedy item followed by nothing required → possessify
  (`a\d+` compiled as `a\d++`, "no point considering backtracking into the digits").
- **Continuation logic:** if the next item *cannot* match empty (`list[1]==0`), one
  disjoint neighbor suffices → TRUE. If it *can* match empty (`x*`), keep scanning to the
  item after it (the optional item could be skipped, handing an interior position to a
  later atom). Walks `OP_ALT` branches, descends into groups (`OP_BRA`/`OP_CBRA`/
  `OP_ONCE`), skips `OP_CALLOUT`, with a recursion-depth bound.

What it emits: single-char/type repeats → `OP_POSSTAR`/`OP_POSPLUS`/`OP_POSQUERY`/
`OP_POSUPTO` (and `OP_TYPEPOS*`); class repeats → `OP_CRPOSSTAR`/`OP_CRPOSPLUS`/
`OP_CRPOSQUERY`/`OP_CRPOSRANGE`. These have no give-back loop — identical to wrapping in
`(?>…)` (`X*+` ≡ `(?>X*)`). Disable via `PCRE2_NO_AUTO_POSSESS` or `(*NO_AUTO_POSSESS)`.

### 4.3 Where it is NOT sound (PCRE2 bailouts)

Unsound whenever something other than the immediately-following required atom could
legitimately consume an interior run position, or an external reference observes the run
boundary:
1. **Following atom optional / can match empty** (`A?`, `A*`, empty-matching group):
   engine may skip `A` and hand an interior position to a later overlapping atom. Tracked
   via `list[1]`; keeps scanning.
2. **Following item overlaps the class:** `\w+\d` is NOT possessifiable (a digit is a
   word char; give-back to the final digit may be the only way to satisfy `\d`). The
   bitset/`autoposstab` overlap path returns FALSE.
3. **Alternation in continuation:** `(A|B)` where any branch matches a `C`-char → not
   disjoint. Walks every `OP_ALT` branch; one overlap blocks it.
4. **Capturing group referenced by recursion/subroutine** (`OP_RECURSE`, `(?R)`,
   `(?1)`): bails for `OP_CBRA/SCBRA/CBRAPOS/SCBRAPOS` when `had_recurse`; cautious about
   a capturing group just before end-of-pattern (may be referenced).
5. **Lookaround / assertions:** variable-length lookbehind (`OP_VREVERSE`) and non-atomic
   assertions (`OP_ASSERT_NA`, `OP_ASSERTBACK_NA`) — an assertion can re-observe input at
   the boundary.
6. **Backreferences:** `\1` observes which chars the group captured; changing run length
   (or relying on disjointness across a backreferenced group) is unsafe.
7. **`OP_SCRIPT_RUN`** and other context-sensitive constructs bail unless trivially a
   repeated explicit char.

Unifying rule: sound iff **no execution path other than maximal-munch-then-required-atom
can produce a match** — nothing downstream validly consumes a swallowed char, and nothing
external observes the run's exact extent.

### 4.4 Relationship to catastrophic-backtracking elimination

Catastrophic backtracking arises when nested/adjacent give-back loops multiply: `(a+)+b`
or `a+a+b` on `aaaa…` (no trailing `b`) explores exponential/quadratic partitionings of
the `a`-run before failing, because each outer give-back re-triggers the inner give-back
(Cox: backtrackers hit `O(2^n)` on `a?^n a^n`, failure cost dominated by give-back states
that can never match).

Auto-possessification attacks this at the leaf: every redundant give-back loop (proven via
disjointness with the required follower) is deleted, collapsing that quantifier's
branching factor from "release one char at a time" to "exactly one position." When the
loop was a *factor* in a multiplicative explosion, removing it can drop the pattern from
exponential/quadratic to linear failure cost. Real-world wins: `"[^"]*"` → `"[^"]*+"`
(string-literal scanning linear); `\w+@\w+\.\w+`-style (each `\w+` whose follower is
disjoint becomes possessive, killing partitioning blowup); `a\d+` → `a\d++`.

Partial defense only: it cannot fix blowup from *overlapping* adjacent quantifiers
(`\w+\w+`, `(a+)+`) where give-back is genuinely needed and disjointness fails — those
need author-side atomic restructuring or a non-backtracking engine (Thompson/Pike VM),
which has no backtracking stack to explode. Auto-possessification is the backtracking
engine's locally-sound approximation of that guarantee.

### 4.5 Applicability to a Pike VM / NFA engine (otter context)

A pure Thompson/Pike VM does not backtrack → no give-back loop to eliminate;
auto-possessification per se is a *backtracking-engine* optimization. Its value for a
non-backtracking engine is different but real: the **same disjointness analysis** can (a)
emit *fewer thread-split states* — skip the "stop repeating here" alternative when it
provably cannot match the follower, shrinking the active thread set per step — and (b)
drive a prefilter / one-pass classification, since a possessive run with a disjoint
terminator is a deterministic split-free segment. The soundness argument (Section 4.1)
transfers verbatim: it is a statement about which positions the follower can match,
independent of execution strategy.

**Sources:** PCRE2 `pcre2_auto_possess.c` —
https://github.com/PCRE2Project/pcre2/blob/main/src/pcre2_auto_possess.c ;
PCRE2 pattern doc (auto-possessification, `a\d++`, `PCRE2_NO_AUTO_POSSESS`) —
https://www.pcre.org/current/doc/html/pcre2pattern.html ;
regular-expressions.info (possessive / atomic equivalence / catastrophic) —
https://www.regular-expressions.info/possessive.html ,
https://www.regular-expressions.info/catastrophic.html ;
rexegg — https://www.rexegg.com/regex-quantifiers.php ;
Russ Cox — https://swtch.com/~rsc/regexp/regexp1.html ;
Friedl, *Mastering Regular Expressions* (atomic grouping / possessive / "introducing
failure cheaply").

(Caveat: some opcode/table names — `autoposstab`, `propposstab`, `catposstab`, the
`OP_*POS*`/`OP_CRPOS*` families, the `had_recurse`/`OP_VREVERSE`/`OP_ASSERT_NA` bailouts —
were paraphrased from WebFetch summaries; the top-of-file and per-function comment quotes
are verbatim. Verify table contents and `compare_opcodes` control flow against the raw
file (~1,200 lines) before implementing.)

---

## 5. Synthesis: levers for otter-regex

The four engines converge on the same principles. In priority order for `otter-regex`
(currently a Pike-VM-direction engine per project memory; bottlenecks = caps.clone per
Split, class binary-search, weak prefilter):

1. **Literal prefilter front-end (biggest, engine-agnostic win).** Extract required
   literals (prefix / suffix / inner) from the pattern and narrow candidate positions
   *before* running the NFA/Pike VM. Single literal → memchr-style two-rarest-bytes SIMD
   scan; multiple → Teddy/Aho-Corasick. This is what RE2 (`PrefixAccel`/shift DFA) and
   Rust regex (`hir::literal` + memmem/Teddy) do, and what V8/YARR approximate with the
   Boyer-Moore skip table. Pick the *rarest* anchor char (YARR even samples the actual
   subject — `SubjectSampler` — to choose).

2. **Per-offset character-set lookahead + skip table** (V8 `BoyerMooreLookahead` /
   `EmitSkipInstructions`, YARR `BoyerMooreInfo`). Collect, per offset in a window of size
   `EatsAtLeast`/`m_minimumSize`, the set of chars that can appear (intersected across
   alternatives); fast-forward past positions that fail the membership test. Use a
   128-bit nibble bitmap so the test is a single SIMD op.

3. **Quick-check mask/value over the first N chars** (V8 `QuickCheckDetails`): one
   load+AND+CMP rejecting non-matching positions before entering the matcher.

4. **Minimum-match-length bound** (V8 `EatsAtLeast`, YARR `m_minimumSize`): precompute per
   node/alternative; never attempt a start index with too little input remaining; do one
   up-front bounds check covering K chars.

5. **Disjointness / possessification** (Section 4): when `C+` is followed by a disjoint
   required atom, skip the give-back. In a Pike VM this means **emitting fewer thread-
   split states** (drop the "stop repeating" alternative), directly cutting the
   per-Split `caps.clone` cost the memory notes flag as the bottleneck. V8's greedy-loop
   fixed-back-count and YARR's `optimizePossessiveQuantifiers` are the two reference
   implementations. (Requires fixed-length body + disjoint continuation.)

6. **Split bounds-finding from capture-resolution** (RE2 + Rust regex): if/when a fast
   span-finder exists (lazy DFA), find match bounds first, then run the capture-tracking
   engine only on the narrow matched span — this is the single largest reason PikeVM-only
   designs are slow, and the cleanest path to closing the gap vs backtrackers on simple
   linear patterns. The one-pass NFA (never copies the submatch boundary set) is the
   capture engine to prefer on the deterministic subset.

7. **Anchoring short-circuits**: sticky (`y`) / `^`-anchored patterns must try only valid
   start positions, never every index (YARR `optimizeBOL` + `onceThrough`, V8 multiline
   handling).

---

## 6. Chosen plan and results (implemented)

Profiling `benchmarks/scripts/regex.js` per operation confirmed the residual was
`exec` (~185ms), ~87% in the matcher: the futile give-back re-scan of
`/([a-z.]+)@([a-z.]+)/gi` (greedy class run, required disjoint `@`). Of the
ranked levers, the two contained / high-ROI ones were implemented; the lazy-DFA
portfolio (lever 4) was not needed.

### 6.1 Auto-possessification via class disjointness (§4)

Lowering pass (`ir.rs::mark_possessive`) marks a greedy fused `Insn::Repeat`
**possessive** when the unique required atom that must follow it — reached through
only zero-width bookkeeping and unconditional `Jump`s (no `Split`, assertion,
lookaround, backreference, `min==0` repeat, or pattern end) — matches a set of
code points disjoint from the repeat's atom. Disjointness is decided by
enumerating the repeat's atom code points (bounded at 2048; huge/negated sets are
skipped) and testing each against the follower with the **executor's own
`char_eq`/`class_member` predicate**, so `/i` case folding never drifts from
execution. The matcher (`backtrack.rs`) then skips pushing the give-back frame for
a possessive repeat. Soundness proof in §4.1.

### 6.2 Leading-run skip (the asymptotic fix)

Possessification removes the give-back frames but the leftmost scan still
restarts one position into the same run (O(run²) per run). New
`Program::lead_possessive_run` is set when the unique first consuming instruction
is a possessive `min>=1` repeat. On a failed attempt the iterator
(`api.rs::Matches::next`) then skips the **entire** maximal run of prefilter
members in one step instead of retrying each interior position — every interior
start fails identically (repeat consumes to the same boundary, never gives back,
disjoint follower fails there). This reuses the existing first-set `Prefilter`
membership test and is sound regardless of any prefilter/atom approximation
(every prefilter-positive interior start provably fails). O(run²) → O(run).

### 6.3 Measured impact (release, this machine)

`exec` phase of `regex.js`: **185ms → 55ms (3.4×)**; whole-script regex.js work
**192ms → 61ms (3.1×)**. `match`/`replace` already used native batch fast paths
(unchanged: ~4ms / ~2ms). Verification: `otter-regex` 32 tests pass (3 new
possessification tests, incl. the overlap case that must *not* possessify);
`otter-vm` 613 lib tests pass; **test262 `RegExp` and `String/prototype` failing
sets byte-identical to baseline (0 regressions)**; `OTTER_GC_STRESS=64` clean;
`OTTER_JIT=0`/`=1` byte-identical on all 11 benchmark scripts.

### 6.4 Remaining gap and next lever (NOT a regex-engine problem)

The matcher is no longer the `exec` bottleneck. The residual ~55ms is host-side:
`RegExp.prototype.exec`/`test` call `JsString::to_utf16_vec` which
**re-materializes the entire subject** into a fresh `Vec<u16>` on *every* call
(`regexp_prototype.rs:72`). For a `/g` exec/test loop that is O(text) per call ×
matches = O(text·matches). Proof: a `.test()` loop (no result-object build) is
just as slow (55ms) as `.exec()`, while `String.prototype.match` (one
materialization, native batch) does the same 3000 matches in ~2ms. The next lever
is a VM/string change — flatten/cache the subject's UTF-16 (or search the string
body zero-copy) — in `crates/otter-vm/src/string` + `regexp_prototype.rs`, not in
`otter-regex`. Lazy-DFA bounds-finding (§3) is unnecessary for these linear
patterns now that the re-scan is gone.
