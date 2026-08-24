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
//! - [`LiteralPrefix`] — the literal run every match must begin with, when the
//!   pattern has one.
//!
//! # Invariants
//! - Operands that reference other instructions are indices into the same
//!   [`Program::insns`] vector.
//! - Every scan aid ([`Program::prefilter`], [`Program::literal_prefix`],
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
    /// The literal code units every match must begin with, when there are at
    /// least two of them. Strictly stronger than [`Self::prefilter`] (which
    /// characterizes only the first code point), so the leftmost search prefers
    /// it: a whole literal run is confirmed by the scan before the matcher is
    /// entered at all.
    pub(crate) literal_prefix: Option<LiteralPrefix>,
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
    /// Membership for code units `0..TABLE` (covers ASCII and Latin-1).
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
        Self {
            table,
            has_high,
            high: set.clone(),
            single,
            canon,
        }
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

/// A literal code-unit sequence every match must begin with, plus the offset of
/// its rarest unit.
///
/// The scan searches for that one unit rather than the first, then confirms the
/// whole literal. Searching a single unit keeps the inner loop a plain equality
/// scan the compiler vectorizes, while picking the *rarest* unit is what makes
/// the candidate rate low: in ordinary text the leading unit of a word is a
/// common letter, so a first-unit scan stops constantly, whereas the `q` of
/// `unquestionable` almost never fires.
#[derive(Debug, Clone)]
pub(crate) struct LiteralPrefix {
    /// The required units, in subject order.
    units: Box<[u16]>,
    /// Index into `units` of the unit the scan searches for.
    rare: usize,
}

impl LiteralPrefix {
    /// Longest literal this scan will hold. A longer required prefix is
    /// truncated, which only weakens the filter, never its soundness.
    const MAX_UNITS: usize = 4096;

    /// Build the scan for a required literal of two or more units.
    pub(crate) fn new(mut units: Vec<u16>) -> Self {
        units.truncate(Self::MAX_UNITS);
        debug_assert!(units.len() >= 2);
        let rare = units
            .iter()
            .enumerate()
            .min_by_key(|(_, u)| unit_commonness(**u))
            .map_or(0, |(i, _)| i);
        Self {
            units: units.into_boxed_slice(),
            rare,
        }
    }

    /// The first offset at or after `from` where the literal occurs, or `None`
    /// when the subject holds no further occurrence.
    #[must_use]
    pub(crate) fn find(&self, text: &[u16], from: usize) -> Option<usize> {
        let n = self.units.len();
        let target = self.units[self.rare];
        let mut i = from;
        loop {
            if i + n > text.len() {
                return None;
            }
            // The searched unit sits `rare` units into the window, so a hit at
            // `i + rare + off` corresponds to a window starting at `i + off`.
            let off = text[i + self.rare..].iter().position(|&u| u == target)?;
            let start = i + off;
            if start + n > text.len() {
                return None;
            }
            if text[start..start + n] == *self.units {
                return Some(start);
            }
            i = start + 1;
        }
    }
}

/// A rough relative frequency for a code unit in ordinary text, used only to
/// pick which unit of a literal the scan searches for. Lower is rarer.
///
/// The exact numbers matter little — what matters is that letters and spaces
/// rank far above punctuation, digits, and everything outside ASCII, so a
/// literal containing an unusual unit searches on that one.
fn unit_commonness(u: u16) -> u8 {
    const TABLE: [u8; 128] = {
        let mut t = [8u8; 128];
        // Punctuation and control units are rarer than letters but not rare
        // enough to beat a genuinely unusual unit.
        let mut i = 0;
        while i < 128 {
            t[i] = if i >= b'a' as usize && i <= b'z' as usize {
                40
            } else if i >= b'A' as usize && i <= b'Z' as usize {
                20
            } else if i >= b'0' as usize && i <= b'9' as usize {
                16
            } else {
                6
            };
            i += 1;
        }
        // The most common English letters, plus the separators that dominate
        // any real subject.
        t[b' ' as usize] = 100;
        t[b'\n' as usize] = 60;
        t[b'e' as usize] = 90;
        t[b't' as usize] = 85;
        t[b'a' as usize] = 82;
        t[b'o' as usize] = 80;
        t[b'i' as usize] = 78;
        t[b'n' as usize] = 76;
        t[b's' as usize] = 74;
        t[b'r' as usize] = 72;
        t[b'h' as usize] = 70;
        t[b'l' as usize] = 65;
        t[b'd' as usize] = 62;
        t[b'u' as usize] = 58;
        t[b'c' as usize] = 55;
        t[b'm' as usize] = 52;
        t
    };
    let i = usize::from(u);
    // Outside ASCII a unit is rare enough that searching it is always the best
    // choice available.
    if i < TABLE.len() { TABLE[i] } else { 1 }
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
        match (&self.prefilter, self.lead_possessive_run) {
            (None, _) => "unfiltered".to_string(),
            (Some(pf), run) => {
                let base = match pf.single() {
                    Some(u) => format!("literal U+{u:04X}"),
                    None => "first-set".to_string(),
                };
                if run {
                    format!("{base} + possessive-run skip")
                } else {
                    base
                }
            }
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
