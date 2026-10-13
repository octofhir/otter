//! Compiled instruction program — the matcher bytecode.
//!
//! A flat, cache-friendly instruction vector produced by [`crate::ir`] lowering
//! and consumed by the backtracking executor. Keeping the program flat (indices,
//! not pointers) makes it cheap to clone, share, and iterate, and lets lookaround
//! bodies live in the same vector as self-contained regions reached only via a
//! [`Insn::Look`] sub-search.
//!
//! # Contents
//! - [`Program`] — the instruction vector plus capture/loop metadata, the
//!   engine-relevant flag bits, and the scan strategy chosen for the pattern.
//! - [`Insn`] — a single matcher instruction.
//! - [`Prefilter`] — the set of code points that can begin a match.
//! - [`RequiredLiteral`] — a literal run every match must contain at a bounded
//!   distance from its start, when the pattern has one.
//!
//! # Invariants
//! - Operands that reference other instructions are indices into the same
//!   [`Program::insns`] vector.
//! - Every scan aid ([`Program::prefilter`], [`Program::required_literal`],
//!   [`Program::start_anchored`]) is a *necessary* condition on a match's start
//!   position, never a sufficient one: skipping a position it rejects can never
//!   discard a match, and every position it accepts is still run through the
//!   matcher.
//! - Capture slots number `2 * (group_count + 1)`: slots `2*g` / `2*g+1` hold
//!   the start / end of group `g`, with group `0` the overall match.
//! - A lookaround body is a contiguous region beginning at [`Insn::Look`]'s
//!   `entry` and terminated by [`Insn::LookMatch`]; normal control flow never
//!   falls into it (a [`Insn::Jump`] hops over it).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

use crate::classes::{ClassSet, CodePointSet};

/// A single matcher instruction.
#[derive(Debug, Clone)]
pub(crate) enum Insn {
    /// Match one literal code point; case-folded comparison when
    /// `ignore_case` (the per-node effective `i` flag) is set.
    Char {
        /// The literal code point.
        cp: u32,
        /// `true` for a case-insensitive comparison.
        ignore_case: bool,
    },
    /// Match one code point against a class set; `negate` inverts membership.
    Class {
        /// Index into [`Program::classes`] of the set tested against the current
        /// code point. Held out-of-line so the instruction stays small and the
        /// program vector dense — class sets are the only large operand.
        class: u32,
        /// `true` for a negated class `[^...]`.
        negate: bool,
        /// `true` for case-insensitive class membership.
        ignore_case: bool,
    },
    /// Match a fixed run of two or more case-sensitive BMP literal code units in
    /// one dispatch (a slice comparison), instead of one `Char` per unit.
    CharSeq(Box<[u16]>),
    /// Match any character (line terminators excluded unless `dot_all`).
    AnyChar {
        /// Whether the `s` (dotAll) flag is in effect.
        dot_all: bool,
    },
    /// A fused unbounded repeat of a single atom (`a+`, `\w*`, `.{2,}`, …) in
    /// non-Unicode mode, where every atom is exactly one code unit. The matcher
    /// consumes the atom in a tight loop instead of dispatching a split-loop per
    /// character, and backtracks by giving back one unit at a time.
    Repeat {
        /// The single-code-unit atom matched repeatedly.
        atom: RepeatAtom,
        /// Mandatory minimum repetitions (`+` → 1, `*` → 0, `{n,}` → n).
        min: u32,
        /// `true` for a greedy repeat (longest first), `false` for lazy.
        greedy: bool,
        /// `true` when the greedy repeat needs no give-back: every character it
        /// can consume is provably disjoint from the unique required atom that
        /// must follow, so no shorter match can ever satisfy the continuation
        /// (auto-possessification — §22.2.2 semantics are unchanged because the
        /// only position the follower can match is the run boundary, exactly
        /// what the maximal munch already exposes). Set by lowering's
        /// possessification pass; always `false` for a lazy repeat.
        possessive: bool,
    },
    /// Never matches. Emitted for a counted quantifier whose minimum exceeds
    /// what any subject could supply, which §22.2.1 permits to be written.
    Fail,
    /// Unconditional jump to an instruction index.
    Jump(usize),
    /// Try the first target; on backtrack, resume at the second.
    Split(usize, usize),
    /// Record the current position into capture slot `index`.
    Save(usize),
    /// Clear the start/end slots for capture group `index` before a repeated
    /// body tries a fresh iteration.
    ClearCapture(u32),
    /// Store the current position into loop-mark slot `index`, marking the start
    /// of an unbounded-quantifier iteration.
    SetMark(usize),
    /// Fail this path if the position equals loop-mark slot `index`: the loop
    /// body matched the empty string, so re-iterating cannot progress
    /// (§22.2.2.5.1 prevents the infinite loop).
    CheckProgress(usize),
    /// Match the text previously captured by one of `indices` (backreference).
    BackRef {
        /// 1-based group indices this resolves to. Duplicate named
        /// backreferences try the capture that participated; if none did, the
        /// backreference matches the empty string. Boxed to keep the
        /// instruction small.
        indices: Box<[u32]>,
        /// `true` for a case-insensitive comparison.
        ignore_case: bool,
    },
    /// `^` — start of input, or of a line when `multiline` is set.
    AssertStart {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `$` — end of input, or of a line when `multiline` is set.
    AssertEnd {
        /// `true` when the `m` (multiline) flag is in effect here.
        multiline: bool,
    },
    /// `\b` / `\B` — word boundary. `invert` selects `\B`; `ignore_case`
    /// widens the word-character set to the two non-ASCII code points
    /// that case-fold into it (only observable under the `u` flag).
    WordBoundary { invert: bool, ignore_case: bool },
    /// Lookaround: run a sub-search of the body at `entry`. `behind` selects
    /// lookbehind, `negate` selects the negative form.
    Look {
        /// `true` for a negative assertion (`(?!`, `(?<!`).
        negate: bool,
        /// `true` for lookbehind (`(?<=`, `(?<!`).
        behind: bool,
        /// First instruction of the lookaround body (terminated by `LookMatch`).
        entry: usize,
    },
    /// Accepting terminator of a lookaround body.
    LookMatch,
    /// Accepting terminator of the whole pattern.
    Match,
}

/// The single-code-unit atom of a fused [`Insn::Repeat`].
#[derive(Debug, Clone)]
pub(crate) enum RepeatAtom {
    /// One literal code unit; case-folded when `ignore_case`.
    Char { cp: u32, ignore_case: bool },
    /// One code unit tested against [`Program::classes`]`[class]`; `negate`
    /// inverts membership.
    Class {
        class: u32,
        negate: bool,
        ignore_case: bool,
    },
    /// Any code unit (line terminators excluded unless `dot_all`).
    Any { dot_all: bool },
}

/// A compiled program ready for execution.
#[derive(Debug, Clone)]
pub(crate) struct Program {
    /// The flat instruction vector; execution begins at index `0`.
    pub(crate) insns: Vec<Insn>,
    /// Out-of-line class sets, indexed by [`Insn::Class::class`]. Kept here so
    /// the instruction vector stays small and cache-dense.
    pub(crate) classes: Vec<ClassSet>,
    /// Number of capturing groups (group 0 excluded).
    pub(crate) group_count: u32,
    /// Resolved capture-group names in source order, the empty string for an
    /// unnamed group; index `i` names group `i + 1`. Shared (`Arc`) so each
    /// produced [`crate::Match`] clones a pointer, not the whole name list.
    pub(crate) names: std::sync::Arc<[String]>,
    /// `u`/`v` — code-point (surrogate-pair-aware) traversal.
    pub(crate) unicode: bool,
    /// Number of loop-mark slots (one per unbounded quantifier), allocated after
    /// the capture slots.
    pub(crate) loop_marks: usize,
    /// Scan prefilter for the leftmost search: the set of code points that can
    /// begin a match, when the pattern starts with a single literal or
    /// non-negated class (including a leading alternation of such). Positions
    /// whose code point is not a member cannot start a match, so the executor
    /// skips them without running. `None` when no such prefilter applies
    /// (anchored, empty-matching, or an uncharacterizable leading instruction).
    pub(crate) prefilter: Option<Prefilter>,
    /// `true` when the unique first consuming instruction (reached from entry
    /// through only zero-width bookkeeping and jumps — no branch or assertion) is
    /// a possessive greedy repeat with `min >= 1`. Every start position inside a
    /// run of that repeat's characters then fails identically (the repeat
    /// consumes to the same run boundary, never gives back, and its disjoint
    /// follower fails at that boundary), so on a failed attempt the leftmost
    /// search skips the entire run in one step instead of retrying each interior
    /// position — turning the per-run cost from O(run²) to O(run). The run is
    /// exactly the maximal span of [`Prefilter`] members, so the skip reuses the
    /// prefilter membership test.
    pub(crate) lead_possessive_run: bool,
    /// A literal run of at least two code units every match must contain, a
    /// bounded number of units after its start. The leftmost search prefers it
    /// over [`Self::prefilter`] (which characterizes only the first code
    /// point): a subject without the literal is rejected by one scan, and only
    /// starts within the literal's offset window of an occurrence are ever run
    /// through the matcher.
    pub(crate) required_literal: Option<RequiredLiteral>,
    /// `true` when every path from entry asserts start-of-input (`^` outside
    /// multiline) before consuming anything. Such a pattern can only match at
    /// offset `0`, so the leftmost search tries that one position and stops
    /// instead of retrying — and rejects a resumed search (`lastIndex > 0`)
    /// outright.
    pub(crate) start_anchored: bool,
}

/// A scan prefilter: the set of code points that can begin a match, in a form
/// the leftmost search dispatches on cheaply.
///
/// Replaces the per-position binary search over code-point ranges with an O(1)
/// table lookup for code units below `TABLE`, and offers a single-literal fast
/// path that the leftmost search turns into a vectorizable equality scan.
#[derive(Debug, Clone)]
pub(crate) struct Prefilter {
    /// Whether each raw input code unit `0..TABLE` (ASCII and Latin-1) can
    /// begin a match. For an `i`-flag prefilter the case fold is already
    /// applied, so the scan never folds a low unit.
    table: [bool; Self::TABLE],
    /// Whether any member code point is `>= TABLE`; when `false`, a code unit
    /// at or above the table can never start a match.
    has_high: bool,
    /// The full set, consulted only for code points `>= TABLE` when `has_high`.
    high: CodePointSet,
    /// `Some(u)` when the set is exactly one BMP, non-surrogate code point: the
    /// scan reduces to a single-unit equality search. Always `None` when
    /// [`Self::canon`] is set (the input must be canonicalized first).
    single: Option<u16>,
    /// Case-folding mode for `i`-flag patterns: `Some(true)` folds the input by
    /// the unicode rule, `Some(false)` by the non-unicode rule, before the
    /// membership test — the stored set already holds canonicalized members, so
    /// this mirrors `char_eq` exactly. `None` for case-sensitive prefilters.
    canon: Option<bool>,
    /// The bytes that can start a match in a one-byte subject, when there
    /// are at most three of them: the table's members.
    few_latin1: smallvec::SmallVec<[u8; 3]>,
}

impl Prefilter {
    const TABLE: usize = 256;

    /// Build a case-sensitive prefilter from a first-set. Cheap; runs once at
    /// lowering.
    #[must_use]
    pub(crate) fn from_set(set: &CodePointSet) -> Self {
        Self::build(set, None)
    }

    /// Build a case-insensitive prefilter: `set` holds the canonicalized member
    /// code points and the scan canonicalizes each input code point by the
    /// `unicode` rule before testing membership.
    #[must_use]
    pub(crate) fn from_set_canon(set: &CodePointSet, unicode: bool) -> Self {
        Self::build(set, Some(unicode))
    }

    fn build(set: &CodePointSet, canon: Option<bool>) -> Self {
        let mut table = [false; Self::TABLE];
        let mut has_high = false;
        for r in set.ranges() {
            let hi = *r.end();
            for cp in *r.start()..=hi.min(Self::TABLE as u32 - 1) {
                table[cp as usize] = true;
            }
            if hi >= Self::TABLE as u32 {
                has_high = true;
            }
        }
        // Fold the low units once here instead of once per scanned position.
        if let Some(unicode) = canon {
            let members = table;
            for (unit, slot) in table.iter_mut().enumerate() {
                let folded = if unicode {
                    crate::casefold::fold_unicode(unit as u32)
                } else {
                    crate::casefold::canonicalize(unit as u32)
                };
                *slot = if (folded as usize) < Self::TABLE {
                    members[folded as usize]
                } else {
                    set.contains(folded)
                };
            }
        }
        let single = match (canon, set.ranges()) {
            (None, [r])
                if r.start() == r.end()
                    && *r.start() < 0x1_0000
                    && !(0xD800..=0xDFFF).contains(r.start()) =>
            {
                Some(*r.start() as u16)
            }
            _ => None,
        };
        let members = table.iter().filter(|&&member| member).count();
        let few_latin1 = if (1..=3).contains(&members) {
            (0..=u8::MAX)
                .filter(|&unit| table[usize::from(unit)])
                .collect()
        } else {
            smallvec::SmallVec::new()
        };
        Self {
            table,
            has_high,
            high: set.clone(),
            single,
            canon,
            few_latin1,
        }
    }

    /// The one to three bytes that can start a match in a one-byte subject.
    #[must_use]
    pub(crate) fn few_latin1(&self) -> Option<&[u8]> {
        (!self.few_latin1.is_empty()).then_some(self.few_latin1.as_slice())
    }

    /// Whether a code unit at or above `0x100` can ever begin a match, so a
    /// scan must look past the low table for it.
    #[must_use]
    pub(crate) fn may_start_above_latin1(&self) -> bool {
        self.has_high || self.canon.is_some()
    }

    /// The single-literal code unit, when the prefilter is one BMP literal.
    #[must_use]
    pub(crate) fn single(&self) -> Option<u16> {
        self.single
    }

    /// Whether decoded code point `cp` can begin a match. Canonicalizes first
    /// for an `i`-flag prefilter. O(1) for the common BMP range; a set test only
    /// for high code points when the set has any.
    #[inline]
    #[must_use]
    pub(crate) fn cp_may_start(&self, cp: u32) -> bool {
        if (cp as usize) < Self::TABLE {
            return self.table[cp as usize];
        }
        let cp = match self.canon {
            Some(true) => crate::casefold::fold_unicode(cp),
            Some(false) => crate::casefold::canonicalize(cp),
            None => cp,
        };
        if (cp as usize) < Self::TABLE {
            self.table[cp as usize]
        } else {
            self.has_high && self.high.contains(cp)
        }
    }
}

/// A literal code-unit sequence every match must contain and the window of
/// distances from the match start at which it can sit.
///
/// The scan is a vectorized substring search for the whole literal over the
/// subject's bytes: the literal's bytes for a one-byte subject, its
/// little-endian unit bytes for a two-byte one, where a hit at an odd byte
/// offset straddles two units and is skipped.
///
/// A literal the pattern begins with has a window of `0..=0`. One behind a
/// bounded-width head — `(^|[^\\])"Date(` — sits `min_offset..=max_offset`
/// units in, so an occurrence at `q` admits only the starts
/// `q - max_offset ..= q - min_offset`.
#[derive(Debug, Clone)]
pub(crate) struct RequiredLiteral {
    /// The required units, in subject order.
    units: Box<[u16]>,
    /// Fewest code units any match consumes before the literal begins.
    pub(crate) min_offset: usize,
    /// Most code units any match consumes before the literal begins.
    pub(crate) max_offset: usize,
    /// The searcher a one-byte subject uses, when every unit of the literal
    /// fits one byte; otherwise the literal never occurs in such a subject.
    narrow: Option<memchr::memmem::Finder<'static>>,
    /// The searcher over a two-byte subject's bytes.
    wide: memchr::memmem::Finder<'static>,
}

impl RequiredLiteral {
    /// Longest literal this scan will hold. A longer required run is
    /// truncated, which only weakens the filter, never its soundness.
    const MAX_UNITS: usize = 4096;

    /// Build the scan for a required literal of two or more units that every
    /// match holds between `min_offset` and `max_offset` units after its start.
    pub(crate) fn new(mut units: Vec<u16>, min_offset: usize, max_offset: usize) -> Self {
        units.truncate(Self::MAX_UNITS);
        debug_assert!(units.len() >= 2);
        debug_assert!(min_offset <= max_offset);
        let narrow = units
            .iter()
            .map(|&unit| u8::try_from(unit).ok())
            .collect::<Option<Vec<u8>>>()
            .map(|bytes| memchr::memmem::Finder::new(&bytes).into_owned());
        let wide_bytes: Vec<u8> = units.iter().flat_map(|unit| unit.to_le_bytes()).collect();
        let wide = memchr::memmem::Finder::new(&wide_bytes).into_owned();
        Self {
            units: units.into_boxed_slice(),
            min_offset,
            max_offset,
            narrow,
            wide,
        }
    }

    /// The literal's code units.
    pub(crate) fn units(&self) -> &[u16] {
        &self.units
    }

    /// The first offset at or after `from` where the literal occurs, or `None`
    /// when the subject holds no further occurrence.
    #[must_use]
    pub(crate) fn find<U: crate::cursor::Unit>(&self, text: &[U], from: usize) -> Option<usize> {
        let bytes = U::as_bytes(text.get(from..)?);
        if !U::WIDE {
            return self.narrow.as_ref()?.find(bytes).map(|at| from + at);
        }
        let mut at = 0;
        loop {
            let hit = at + self.wide.find(&bytes[at..])?;
            if hit % 2 == 0 {
                return Some(from + hit / 2);
            }
            at = hit + 1;
        }
    }
}

impl Program {
    /// Number of slots: capture slots `2 * (group_count + 1)` plus one per
    /// unbounded-quantifier progress mark.
    #[must_use]
    pub(crate) fn slot_count(&self) -> usize {
        2 * (self.group_count as usize + 1) + self.loop_marks
    }
}

impl Program {
    /// Render the lowered program as a human-readable listing: one line per
    /// instruction, preceded by the scan strategy chosen for it.
    ///
    /// This is a diagnostic surface for the measurement harness, not part of
    /// the matching contract; the exact text is free to change.
    pub(crate) fn describe(&self) -> String {
        use core::fmt::Write as _;
        let mut out = String::new();
        let _ = writeln!(
            out,
            "groups   {}\nunicode  {}\nmarks    {}",
            self.group_count, self.unicode, self.loop_marks
        );
        let _ = writeln!(out, "scan     {}", self.scan_strategy());
        for (i, class) in self.classes.iter().enumerate() {
            let _ = writeln!(out, "class#{i}  {}", describe_class(class));
        }
        let _ = writeln!(out, "insns");
        for (pc, insn) in self.insns.iter().enumerate() {
            let _ = writeln!(out, "  {pc:>4}  {}", describe_insn(insn));
        }
        out
    }

    /// A one-line summary of how the leftmost search skips positions.
    fn scan_strategy(&self) -> String {
        let base = if self.start_anchored {
            "anchored".to_string()
        } else if let Some(lit) = &self.required_literal {
            format!(
                "literal {:?} at {}..={}",
                String::from_utf16_lossy(lit.units()),
                lit.min_offset,
                lit.max_offset
            )
        } else {
            match &self.prefilter {
                None => "unfiltered".to_string(),
                Some(pf) => match pf.single() {
                    Some(u) => format!("unit U+{u:04X}"),
                    None => "first-set".to_string(),
                },
            }
        };
        if self.lead_possessive_run && self.prefilter.is_some() {
            format!("{base} + possessive-run skip")
        } else {
            base
        }
    }
}

/// Render a class set compactly: its first few ranges plus any string
/// alternatives.
fn describe_class(set: &ClassSet) -> String {
    use core::fmt::Write as _;
    let mut s = String::new();
    for (i, r) in set.code_points.ranges().iter().enumerate() {
        if i == 6 {
            let _ = write!(s, " …{} more", set.code_points.ranges().len() - 6);
            break;
        }
        if r.start() == r.end() {
            let _ = write!(s, " {:04X}", r.start());
        } else {
            let _ = write!(s, " {:04X}-{:04X}", r.start(), r.end());
        }
    }
    if !set.strings.is_empty() {
        let _ = write!(s, " +{} string(s)", set.strings.len());
    }
    s.trim_start().to_string()
}

/// Render one instruction.
fn describe_insn(insn: &Insn) -> String {
    match insn {
        Insn::Char { cp, ignore_case } => {
            format!("char U+{cp:04X}{}", if *ignore_case { " /i" } else { "" })
        }
        Insn::CharSeq(seq) => {
            let text: String = char::decode_utf16(seq.iter().copied())
                .map(|c| c.unwrap_or('\u{FFFD}'))
                .collect();
            format!("charseq {} {text:?}", seq.len())
        }
        Insn::Class {
            class,
            negate,
            ignore_case,
        } => format!(
            "class#{class}{}{}",
            if *negate { " negated" } else { "" },
            if *ignore_case { " /i" } else { "" }
        ),
        Insn::AnyChar { dot_all } => {
            format!("any{}", if *dot_all { " dotall" } else { "" })
        }
        Insn::Repeat {
            atom,
            min,
            greedy,
            possessive,
        } => format!(
            "repeat {} min={min}{}{}",
            match atom {
                RepeatAtom::Char { cp, ignore_case } =>
                    format!("char U+{cp:04X}{}", if *ignore_case { " /i" } else { "" }),
                RepeatAtom::Class {
                    class,
                    negate,
                    ignore_case,
                } => format!(
                    "class#{class}{}{}",
                    if *negate { " negated" } else { "" },
                    if *ignore_case { " /i" } else { "" }
                ),
                RepeatAtom::Any { dot_all } => {
                    format!("any{}", if *dot_all { " dotall" } else { "" })
                }
            },
            if *greedy { " greedy" } else { " lazy" },
            if *possessive { " possessive" } else { "" }
        ),
        Insn::Fail => "fail".to_string(),
        Insn::Jump(t) => format!("jump {t}"),
        Insn::Split(a, b) => format!("split {a} {b}"),
        Insn::Save(slot) => format!("save {slot}"),
        Insn::ClearCapture(g) => format!("clearcap {g}"),
        Insn::SetMark(m) => format!("setmark {m}"),
        Insn::CheckProgress(m) => format!("checkprogress {m}"),
        Insn::BackRef {
            indices,
            ignore_case,
        } => format!(
            "backref {indices:?}{}",
            if *ignore_case { " /i" } else { "" }
        ),
        Insn::AssertStart { multiline } => {
            format!("assertstart{}", if *multiline { " /m" } else { "" })
        }
        Insn::AssertEnd { multiline } => {
            format!("assertend{}", if *multiline { " /m" } else { "" })
        }
        Insn::WordBoundary {
            invert,
            ignore_case,
        } => format!(
            "wordboundary{}{}",
            if *invert { " inverted" } else { "" },
            if *ignore_case { " /i" } else { "" }
        ),
        Insn::Look {
            negate,
            behind,
            entry,
        } => format!(
            "look{}{} entry={entry}",
            if *behind { " behind" } else { " ahead" },
            if *negate { " negative" } else { "" }
        ),
        Insn::LookMatch => "lookmatch".to_string(),
        Insn::Match => "match".to_string(),
    }
}

#[cfg(test)]
mod scan_tests {
    use super::RequiredLiteral;

    fn naive(text: &[u16], literal: &[u16], from: usize) -> Option<usize> {
        (from..=text.len().saturating_sub(literal.len())).find(|&at| {
            at + literal.len() <= text.len() && &text[at..at + literal.len()] == literal
        })
    }

    #[test]
    fn literal_search_agrees_with_a_naive_scan_at_both_widths() {
        let subject = "xNQ_VQyy-NQ_VQ--NQ_V-NQ_VQ";
        let narrow: Vec<u8> = subject.bytes().collect();
        let wide: Vec<u16> = subject.encode_utf16().collect();
        for literal in ["NQ_VQ", "Q-", "zz", "VQ"] {
            let units: Vec<u16> = literal.encode_utf16().collect();
            let scan = RequiredLiteral::new(units.clone(), 0, 0);
            for from in 0..=wide.len() {
                let expected = naive(&wide, &units, from);
                assert_eq!(scan.find(&narrow, from), expected, "{literal} from {from}");
                assert_eq!(scan.find(&wide, from), expected, "{literal} from {from}");
            }
        }
        // Unit bytes that line up only across a unit boundary are no
        // occurrence: `AB` is `41 00 42 00`, which `[0x4100, 0x4200, 0]`
        // holds from its second byte.
        let straddle = RequiredLiteral::new(vec![0x41, 0x42], 0, 0);
        let shifted: [u16; 5] = [0x4100, 0x4200, 0x0000, 0x0041, 0x0042];
        assert_eq!(straddle.find(&shifted, 0), Some(3));
        assert_eq!(straddle.find(&shifted[..3], 0), None);
        // A literal wider than one byte never occurs in a one-byte subject.
        let wide_literal = RequiredLiteral::new(vec![0x4e00, 0x4e8c], 0, 0);
        assert_eq!(wide_literal.find(&narrow, 0), None);
    }
}
