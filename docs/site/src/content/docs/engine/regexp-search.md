---
title: "RegExp search plans"
---

`otter-regex` compiles the parsed pattern into one instruction program. Each
search selects one execution plan: complete literal, native backtracking matcher,
or interpreted backtracking matcher. The VM owns the JavaScript protocol around
that search, including `lastIndex`, legacy RegExp statics and result objects.

The existing required-literal scan normally proves only a necessary condition:
the matcher must still verify an occurrence. When lowering proves that the
entire program is a case-sensitive `Char` or `CharSeq` followed by `Match`, the
same scan proves both endpoints. This complete plan publishes group zero
directly. It neither compiles a native matcher nor initializes a backtrack stack,
undo log or capture vector.

Admission excludes captures, assertions, branches, case-folded characters and
Unicode mode. The declared capture count must be zero even when `{0}` removed
all capture instructions: the general matcher still publishes those absent
groups. Admission also excludes a single surrogate and a needle exceeding the
scan's 4096-unit bound: a truncated needle cannot prove the whole pattern.
Other programs retain their required-literal or first-set prefilter and matcher.
`Regex::describe()` labels the proof as `complete literal`.

Both Latin-1 and UTF-16 searches preserve code-unit offsets. A single-unit needle
uses the existing unit scanner, avoiding both finder allocation and repeated
half-unit byte hits. Sequence finders live in one pattern-owned preparation box;
neither creating a search nor producing a match allocates that box. Longer
UTF-16 byte scans reject unaligned occurrences.
A leftmost complete scan charges one matcher step
per occurrence. A sticky search first applies the normal start prefilter and
then charges one attempted position, including a position whose first unit
matches but whose remaining literal fails. Exhaustion returns the existing
explicit resource error. Complete scans cannot match the empty string.

Search scratch retains two inline result words for complete scans. Borrowed
views expire on the next search step; owned records and batches keep their
existing capture representation. Searches share no mutable buffers through the
compiled pattern. No subject borrow crosses a VM allocation or JavaScript call.

The complete plan is independent of the host JavaScript JIT architecture. Tests
force the backtracker separately, comparing ranges, captures, errors and work
counts for both subject widths, sticky positions and budgets. JavaScript
differential tests cover the surrounding exec/test/string protocols.
