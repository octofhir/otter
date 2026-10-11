//! The public API surface — [`Regex`], [`Match`], and the match iterator.
//!
//! This is the exact surface a host VM consumes. A
//! [`Regex`] is compiled once from a UTF-16 (or `&str`) pattern plus [`Flags`];
//! [`Regex::find_utf16`] runs it against a UTF-16 subject from a start offset,
//! yielding [`Match`]es. The stateful JS flags `g`/`y`/`d` are handled by the
//! host *above* this API.
//!
//! # Contents
//! - [`Regex`] — a compiled pattern.
//! - [`Match`] — overall range, capture ranges, named-group access.
//! - [`Matches`] — iterator of `Result<Match, ExecError>` over a subject.
//! - [`NamedGroups`] — deterministic, deduplicated named-group iterator.
//!
//! # Invariants
//! - All [`Match`] offsets are UTF-16 code-unit indices.
//! - [`Match::captures`] is 1-based (index 0 = group 1); group 0 is
//!   [`Match::range`].
//! - [`Match::named_groups`] yields source order, deduplicating duplicate names
//!   (ES2025) and preferring the alternative that matched.
//! - [`Match::named_group_index`] owns named-capture selection; equal capture
//!   ranges do not make distinct capture groups interchangeable.
//! - A successful scan always advances `next_start` past the current match, so
//!   the iterator terminates even on zero-width matches.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec> (§22.2.7.2)

use core::ops::Range;

use crate::cursor::{Input, Unit};
use crate::error::{ExecError, RegexError};
use crate::exec::ExecConfig;
use crate::exec::backtrack;
use crate::flags::Flags;
use crate::program::Program;

/// A compiled regular expression.
///
/// Cheap to share by reference; holds the lowered [`Program`] and the flags it
/// was compiled under. Construct with [`Regex::compile_utf16`] (the primary
/// path for UTF-16 hosts) or [`Regex::compile_str`].
#[derive(Debug, Clone)]
pub struct Regex {
    program: Program,
    flags: Flags,
}

impl Regex {
    /// Compile a pattern given as UTF-16 code units. This is the primary entry
    /// point for UTF-16 hosts (e.g. a JS engine): JS-only escapes (`\u{...}`,
    /// surrogate pairs) are preserved exactly with no lossy text conversion.
    pub fn compile_utf16(pattern: &[u16], flags: Flags) -> Result<Self, RegexError> {
        let parsed = crate::parser::parse(pattern, flags)?;
        let program = crate::ir::lower(parsed, flags);
        Ok(Self { program, flags })
    }

    /// Compile a pattern given as a `&str`. Convenience for callers that only
    /// need parse-time validation (e.g. a literal validator).
    pub fn compile_str(pattern: &str, flags: Flags) -> Result<Self, RegexError> {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Self::compile_utf16(&units, flags)
    }

    /// Compile a `&str` pattern. Drop-in name for hosts migrating off an
    /// engine with a `with_flags` constructor; equivalent to [`Regex::compile_str`].
    pub fn with_flags(pattern: &str, flags: Flags) -> Result<Self, RegexError> {
        Self::compile_str(pattern, flags)
    }

    /// The flags this regex was compiled under.
    #[must_use]
    pub fn flags(&self) -> Flags {
        self.flags
    }

    /// Number of capturing groups (group 0 excluded).
    #[must_use]
    pub fn group_count(&self) -> u32 {
        self.program.group_count
    }

    /// Search `text` (UTF-16 code units) from code-unit offset `start`, yielding
    /// successive non-overlapping matches. Each item is `Ok(Match)` or, if the
    /// [`ExecConfig`] step budget is exhausted, exactly one
    /// [`ExecError::StepLimitExceeded`] after which the iterator ends.
    #[must_use]
    pub fn find_utf16<'r, 't>(
        &'r self,
        text: &'t [u16],
        start: usize,
        config: ExecConfig,
    ) -> Matches<'r, 't> {
        self.find_units(text, start, config)
    }

    /// Search a one-byte subject — every code unit below `0x100`, one byte
    /// each — exactly as [`Regex::find_utf16`] searches its UTF-16 form, with
    /// the same code-unit offsets, without widening it.
    #[must_use]
    pub fn find_latin1<'r, 't>(
        &'r self,
        text: &'t [u8],
        start: usize,
        config: ExecConfig,
    ) -> Matches<'r, 't, u8> {
        self.find_units(text, start, config)
    }

    fn find_units<'r, 't, U: Unit>(
        &'r self,
        text: &'t [U],
        start: usize,
        config: ExecConfig,
    ) -> Matches<'r, 't, U> {
        Matches {
            regex: self,
            text,
            next_start: start,
            config,
            done: false,
            steps_executed: 0,
            scratch: backtrack::Scratch::new(),
        }
    }

    /// Render the lowered program as a human-readable listing.
    ///
    /// A diagnostic surface for tooling (the measurement harness dumps it to
    /// explain which scan strategy a pattern chose); the exact text carries no
    /// stability promise.
    #[must_use]
    pub fn describe(&self) -> String {
        self.program.describe()
    }

    /// Drop-in alias for [`Regex::find_utf16`] matching the migrated host's
    /// existing call site.
    #[must_use]
    pub fn find_from_utf16_with_config<'r, 't>(
        &'r self,
        text: &'t [u16],
        start: usize,
        config: ExecConfig,
    ) -> Matches<'r, 't> {
        self.find_utf16(text, start, config)
    }
}

/// How the leftmost search advances between candidate start positions.
///
/// Resolved once from the compiled program, so the per-candidate path is a
/// single dispatch rather than a chain of option tests.
enum Scan<'p> {
    /// Every position is a candidate; the matcher decides.
    Unfiltered,
    /// Only offset 0 can match.
    Anchored,
    /// Candidates are the starts within a required literal's offset window
    /// of one of its occurrences.
    Literal(&'p crate::program::RequiredLiteral),
    /// Candidates are the positions whose code point can begin a match.
    FirstSet(&'p crate::program::Prefilter),
}

/// Iterator over successive matches of a [`Regex`] in a subject of UTF-16
/// units or Latin-1 bytes.
///
/// Mirrors the host's existing consumption: it collects `Result<Match,
/// ExecError>` and stops on the first error. The host preserves that error as
/// resource exhaustion rather than converting it into a semantic no-match.
#[derive(Debug)]
pub struct Matches<'r, 't, U: Unit = u16> {
    regex: &'r Regex,
    text: &'t [U],
    next_start: usize,
    config: ExecConfig,
    done: bool,
    steps_executed: u64,
    scratch: backtrack::Scratch,
}

impl<U: Unit> Matches<'_, '_, U> {
    /// Number of backtrack points explored by this search so far.
    #[must_use]
    pub fn steps_executed(&self) -> u64 {
        self.steps_executed
    }
}

impl<U: Unit> Iterator for Matches<'_, '_, U> {
    type Item = Result<Match, ExecError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let program = &self.regex.program;
        let unicode = program.unicode;
        let input = Input::new(self.text, unicode);
        let mut pos = self.next_start;
        // Resolve the scan strategy once per call rather than re-testing it at
        // every candidate position.
        let scan = if program.start_anchored {
            Scan::Anchored
        } else if let Some(lit) = &program.required_literal {
            Scan::Literal(lit)
        } else if let Some(pf) = &program.prefilter {
            Scan::FirstSet(pf)
        } else {
            Scan::Unfiltered
        };
        loop {
            match scan {
                Scan::Unfiltered => {}
                // A start-anchored pattern (`^` outside multiline) can only
                // match at offset 0: every later position fails the assertion
                // identically, so the whole search ends rather than retrying
                // each one.
                Scan::Anchored => {
                    if pos > 0 {
                        self.done = true;
                        return None;
                    }
                }
                // A match starting at `pos` or later holds the literal at
                // `pos + min_offset` or later, so the first such occurrence `at`
                // rules out every start before `at - max_offset`: a match there
                // would need an earlier occurrence. Jump to that start.
                Scan::Literal(lit) => {
                    match lit.find(self.text, pos.saturating_add(lit.min_offset)) {
                        Some(at) => {
                            let earliest = at - lit.max_offset.min(at);
                            if earliest > pos {
                                pos = earliest;
                                // The leftmost search never starts between the
                                // halves of a surrogate pair; the pair's lead is
                                // the position it would try instead.
                                if unicode
                                    && (0xDC00..=0xDFFF).contains(&self.text[pos].widen())
                                    && (0xD800..=0xDBFF).contains(&self.text[pos - 1].widen())
                                {
                                    pos -= 1;
                                }
                            }
                        }
                        // No further occurrence, so no further match. Step past
                        // the end so the terminator below fires.
                        None => pos = self.text.len() + 1,
                    }
                }
                // First-set prefilter: skip positions that cannot start a match.
                Scan::FirstSet(pf) => {
                    if let Some(unit) = pf.single() {
                        // Single-literal fast path: a vectorizable equality scan for
                        // the one code unit that can begin a match. A unit too
                        // wide for the subject never occurs in it.
                        match U::narrow(unit).and_then(|unit| {
                            self.text
                                .get(pos..)
                                .and_then(|t| t.iter().position(|&u| u == unit))
                        }) {
                            Some(off) => pos += off,
                            // No further occurrence: a single-literal prefilter means
                            // every match must consume that unit, so no match remains.
                            // Step past the end so the terminator below fires (rather
                            // than clamping to `len`, which would oscillate with
                            // `advance_scan`).
                            None => pos = self.text.len() + 1,
                        }
                    } else if unicode {
                        while pos < self.text.len() {
                            let (cp, _) = decode_at(self.text, pos, unicode);
                            if pf.cp_may_start(cp) {
                                break;
                            }
                            pos = advance_scan(self.text, pos, unicode);
                        }
                    } else {
                        // Non-Unicode: a code unit is a code point, so the scan is a
                        // tight single-step loop with no surrogate decode.
                        while pos < self.text.len() && !pf.cp_may_start(self.text[pos].into()) {
                            pos += 1;
                        }
                    }
                }
            }
            if pos > self.text.len() {
                self.done = true;
                return None;
            }
            let remaining = self.config.step_limit.saturating_sub(self.steps_executed);
            let (attempt, steps) = backtrack::attempt(
                &self.regex.program,
                &input,
                pos,
                ExecConfig {
                    step_limit: remaining,
                },
                &mut self.scratch,
            );
            self.steps_executed = self.steps_executed.saturating_add(steps);
            match attempt {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(true) => {
                    let mat = build_match(&self.regex.program, self.scratch.captures());
                    let end = mat.range.end;
                    self.next_start = if end > pos {
                        end
                    } else {
                        advance_scan(self.text, pos, unicode)
                    };
                    return Some(Ok(mat));
                }
                Ok(false) => {
                    // When the unique leading consumer is a possessive `min >= 1`
                    // repeat, every prefilter-positive position in the current run
                    // fails identically, so skip the whole run in one step rather
                    // than retrying each interior position (O(run²) -> O(run)).
                    if let (true, Some(pf)) = (program.lead_possessive_run, &program.prefilter) {
                        let mut e = advance_scan(self.text, pos, unicode);
                        if unicode {
                            while e < self.text.len() {
                                let (cp, _) = decode_at(self.text, e, unicode);
                                if !pf.cp_may_start(cp) {
                                    break;
                                }
                                e = advance_scan(self.text, e, unicode);
                            }
                        } else {
                            while e < self.text.len() && pf.cp_may_start(self.text[e].into()) {
                                e += 1;
                            }
                        }
                        pos = e;
                    } else {
                        pos = advance_scan(self.text, pos, unicode);
                    }
                }
            }
        }
    }
}

/// Decode the code point at `pos` (which must be `< text.len()`), returning it
/// and its code-unit width. Surrogate pairs combine only in unicode mode.
fn decode_at<U: Unit>(text: &[U], pos: usize, unicode: bool) -> (u32, usize) {
    let hi: u32 = text[pos].into();
    if unicode
        && (0xD800..=0xDBFF).contains(&hi)
        && let Some(&lo) = text.get(pos + 1)
        && let lo = Into::<u32>::into(lo)
        && (0xDC00..=0xDFFF).contains(&lo)
    {
        return (0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00), 2);
    }
    (hi, 1)
}

/// Advance a scan position by one code point (two units over a surrogate pair in
/// unicode mode) so the leftmost search makes progress.
fn advance_scan<U: Unit>(text: &[U], pos: usize, unicode: bool) -> usize {
    if unicode
        && let Some(&hi) = text.get(pos)
        && (0xD800..=0xDBFF).contains(&hi.widen())
        && let Some(&lo) = text.get(pos + 1)
        && (0xDC00..=0xDFFF).contains(&lo.widen())
    {
        return pos + 2;
    }
    pos + 1
}

/// Build a [`Match`] from filled capture slots.
fn build_match(program: &Program, caps: &[Option<usize>]) -> Match {
    let range = caps[0].unwrap_or(0)..caps[1].unwrap_or(0);
    let mut captures = Vec::with_capacity(program.group_count as usize);
    for g in 1..=program.group_count as usize {
        let entry = match (
            caps.get(2 * g).copied().flatten(),
            caps.get(2 * g + 1).copied().flatten(),
        ) {
            (Some(s), Some(e)) => Some(s..e),
            _ => None,
        };
        captures.push(entry);
    }
    Match {
        range,
        captures,
        group_names: program.names.clone(),
    }
}

/// A single successful match.
///
/// All ranges are UTF-16 code-unit offsets into the searched subject.
#[derive(Debug, Clone)]
pub struct Match {
    /// The overall match extent (group 0). May be empty for a zero-width match.
    pub range: Range<usize>,
    /// Per-capture-group ranges, 1-based: index 0 is group 1. `None` for a
    /// group that did not participate in the match.
    pub captures: Vec<Option<Range<usize>>>,
    /// Capture-group names in source order, parallel to a notional `group N`;
    /// the empty string marks an unnamed group. Shared with the compiled
    /// program (`Arc`), so producing a match clones a pointer, not the names.
    /// Used by [`Match::named_groups`] and [`Match::named_group_index`].
    pub(crate) group_names: std::sync::Arc<[String]>,
}

impl Match {
    /// Access a group by index, group 0 being the overall match.
    #[must_use]
    pub fn group(&self, index: usize) -> Option<Range<usize>> {
        if index == 0 {
            Some(self.range.clone())
        } else {
            self.captures.get(index - 1).and_then(Clone::clone)
        }
    }

    /// Select a named capture's 1-based group index, preferring a matched
    /// alternative when a name appears more than once. If every alternative
    /// is unmatched, return the first declared group with that name.
    #[must_use]
    pub fn named_group_index(&self, name: &str) -> Option<usize> {
        if name.is_empty() {
            return None;
        }
        let mut first = None;
        for (idx, gname) in self.group_names.iter().enumerate() {
            if gname == name {
                if self.captures.get(idx).is_some_and(Option::is_some) {
                    return Some(idx + 1);
                }
                first.get_or_insert(idx + 1);
            }
        }
        first
    }

    /// Access a named group's range, deduplicating duplicate names and
    /// preferring the alternative that matched.
    #[must_use]
    pub fn named_group(&self, name: &str) -> Option<Range<usize>> {
        self.named_group_index(name)
            .and_then(|index| self.group(index))
    }

    /// Iterate the named groups in deterministic source order, each name once.
    #[must_use]
    pub fn named_groups(&self) -> NamedGroups<'_> {
        NamedGroups { mat: self, next: 0 }
    }
}

/// Deterministic, deduplicated iterator over a [`Match`]'s named groups.
///
/// Yields `(name, Option<range>)` in pattern source order, each distinct name
/// exactly once, preferring the matched alternative for duplicate names.
#[derive(Debug)]
pub struct NamedGroups<'m> {
    mat: &'m Match,
    next: usize,
}

impl<'m> Iterator for NamedGroups<'m> {
    type Item = (&'m str, Option<Range<usize>>);

    fn next(&mut self) -> Option<Self::Item> {
        let names = &self.mat.group_names;
        while self.next < names.len() {
            let idx = self.next;
            self.next += 1;
            let name = names[idx].as_str();
            if name.is_empty() {
                continue;
            }
            if names[..idx].iter().any(|n| n == name) {
                continue;
            }
            let range = self.mat.named_group(name);
            return Some((name, range));
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(range: Range<usize>, caps: Vec<Option<Range<usize>>>, names: &[&str]) -> Match {
        Match {
            range,
            captures: caps,
            group_names: names.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    #[test]
    fn group_zero_is_overall_match() {
        let m = mk(2..5, vec![Some(2..3)], &["a"]);
        assert_eq!(m.group(0), Some(2..5));
        assert_eq!(m.group(1), Some(2..3));
        assert_eq!(m.group(2), None);
    }

    #[test]
    fn named_group_lookup() {
        let m = mk(0..3, vec![Some(0..1), None], &["x", "y"]);
        assert_eq!(m.named_group("x"), Some(0..1));
        assert_eq!(m.named_group("y"), None);
        assert_eq!(m.named_group("z"), None);
        assert_eq!(m.named_group(""), None);
    }

    /// ES2025 duplicate named groups: the same name appears in two
    /// alternatives; the matched alternative's range wins.
    #[test]
    fn duplicate_named_group_prefers_match() {
        let m = mk(0..1, vec![None, Some(0..1)], &["dup", "dup"]);
        assert_eq!(m.named_group_index("dup"), Some(2));
        assert_eq!(m.named_group("dup"), Some(0..1));
    }

    #[test]
    fn unmatched_named_group_retains_its_declared_index() {
        let m = mk(0..0, vec![None, None, None], &["dup", "other", "dup"]);
        assert_eq!(m.named_group_index("dup"), Some(1));
        assert_eq!(m.named_group_index("other"), Some(2));
        assert_eq!(m.named_group_index("missing"), None);
        assert_eq!(m.named_group_index(""), None);
        assert_eq!(m.named_group("dup"), None);
        assert_eq!(
            m.named_groups().collect::<Vec<_>>(),
            vec![("dup", None), ("other", None)]
        );
    }

    #[test]
    fn equal_ranges_preserve_distinct_capture_indices() {
        let m = mk(0..1, vec![Some(0..1), Some(0..1)], &["outer", "inner"]);
        assert_eq!(m.named_group_index("outer"), Some(1));
        assert_eq!(m.named_group_index("inner"), Some(2));
        assert_eq!(m.named_group("outer"), m.named_group("inner"));
    }

    #[test]
    fn named_groups_iter_is_deduped_and_ordered() {
        let m = mk(0..1, vec![Some(0..1), None, None], &["dup", "dup", "other"]);
        let collected: Vec<_> = m.named_groups().map(|(n, r)| (n.to_string(), r)).collect();
        assert_eq!(
            collected,
            vec![("dup".to_string(), Some(0..1)), ("other".to_string(), None)]
        );
    }
}

#[cfg(test)]
mod iu_word_tests {
    use super::*;
    #[test]
    fn iu_class_w_extended() {
        let f = Flags {
            ignore_case: true,
            unicode: true,
            ..Default::default()
        };
        let text: Vec<u16> = "ſ".encode_utf16().collect();
        for (pat, expect) in [
            (r"\w", true),
            (r"\W", false),
            (r"[\W]", false),
            (r"[^\W]", true),
        ] {
            let units: Vec<u16> = pat.encode_utf16().collect();
            let re = Regex::compile_utf16(&units, f).unwrap();
            let m = re
                .find_from_utf16_with_config(&text, 0, ExecConfig::default())
                .next()
                .is_some();
            assert_eq!(m, expect, "{pat}");
        }
    }
}
