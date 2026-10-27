//! The public API surface — [`Regex`], [`Match`], and lending match searches.
//!
//! This is the exact surface a host VM consumes. A
//! [`Regex`] is compiled once from a UTF-16 (or `&str`) pattern plus [`Flags`];
//! [`Regex::find_utf16`] runs it against a UTF-16 subject from a start offset,
//! yielding [`Match`]es. The stateful JS flags `g`/`d` are handled by the
//! host *above* this API; the host selects `y` per search with
//! [`Matches::anchored`].
//!
//! # Contents
//! - [`Regex`] — a compiled pattern.
//! - [`Match`] — overall range, capture ranges, named-group access.
//! - [`Matches`] — lending search with borrowed views and owned collection,
//!   leftmost or anchored at its position.
//! - [`crate::NamedGroups`] — deterministic, deduplicated named-group iterator.
//! - [`MatchScratch`] — matcher buffers a host reuses across searches.
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
//!   the search terminates even on zero-width matches.
//! - An anchored search tries only its own position; its first failure ends
//!   it, so it never pays for a later candidate.
//! - Under Unicode semantics a start between the halves of a surrogate pair
//!   searches from the pair's lead.
//! - A regex searched often enough runs its searches as machine code
//!   ([`crate::exec::native`]) when the emitter covers its program; matches,
//!   capture ranges and step counts are exactly the interpreter's.
//! - A search owns its matcher buffers; a host that passes a
//!   [`MatchScratch`] in gets it back from [`Matches::into_scratch`], so
//!   sharing a [`Regex`] never serializes matching.
//! - A complete literal scan writes its two result slots directly, without
//!   matcher buffers or native compilation. Sticky failures charge the same
//!   candidate attempt as the backtracker; unanchored scans charge only hits.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec> (§22.2.7.2)

use std::sync::Arc;

use crate::matches::{Match, MatchBatch, MatchRecord, Slots};

use crate::cursor::{Input, Unit};
use crate::error::{ExecError, RegexError};
use crate::exec::ExecConfig;
use crate::exec::backtrack;
use crate::exec::native::{self, NativeCode, NativeTier, Outcome, Window};
use crate::flags::Flags;
use crate::program::{Program, RequiredLiteral};

mod literal;

#[cfg(test)]
mod literal_tests;

/// A compiled regular expression.
///
/// Cheap to share by reference; holds the lowered [`Program`] and the flags it
/// was compiled under. Construct with [`Regex::compile_utf16`] (the primary
/// path for UTF-16 hosts) or [`Regex::compile_str`].
#[derive(Debug, Clone)]
pub struct Regex {
    program: Program,
    flags: Flags,
    native: NativeTier,
    /// Some capture group has a name.
    named: bool,
}

/// Matcher buffers a host keeps between searches.
///
/// A search takes them by value ([`Regex::find_utf16_with`],
/// [`Regex::find_latin1_with`]) and hands them back with
/// [`Matches::into_scratch`], so a hot `exec`/`test` loop reuses one backtrack
/// stack, undo log and capture array instead of allocating and growing fresh
/// ones per call. Buffers a search grew past a small bound come back empty.
///
/// The buffers live behind one box, so handing them between a host and a
/// search moves a pointer.
#[derive(Debug, Default)]
pub struct MatchScratch(Option<Box<Buffers>>);

/// The buffers of both tiers.
#[derive(Debug, Default)]
struct Buffers {
    interpreter: backtrack::Scratch,
    native: native::Scratch,
    literal: [u64; 2],
}

impl Regex {
    /// Compile a pattern given as UTF-16 code units. This is the primary entry
    /// point for UTF-16 hosts (e.g. a JS engine): JS-only escapes (`\u{...}`,
    /// surrogate pairs) are preserved exactly with no lossy text conversion.
    pub fn compile_utf16(pattern: &[u16], flags: Flags) -> Result<Self, RegexError> {
        let parsed = crate::parser::parse(pattern, flags)?;
        let program = crate::ir::lower(parsed, flags);
        let named = program.names.iter().any(|name| !name.is_empty());
        Ok(Self {
            program,
            flags,
            native: NativeTier::default(),
            named,
        })
    }

    /// Compile a pattern given as a `&str`. Convenience for callers that only
    /// need parse-time validation (e.g. a literal validator).
    pub fn compile_str(pattern: &str, flags: Flags) -> Result<Self, RegexError> {
        let units: Vec<u16> = pattern.encode_utf16().collect();
        Self::compile_utf16(&units, flags)
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

    /// Whether some capture group has a name.
    #[must_use]
    pub fn has_named_groups(&self) -> bool {
        self.named
    }

    /// Capture-group names in source order, the empty string for an unnamed
    /// group; index `i` names group `i + 1`.
    #[must_use]
    pub fn group_names(&self) -> &Arc<[String]> {
        &self.program.names
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
        self.find_units(text, start, config, MatchScratch::default())
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
        self.find_units(text, start, config, MatchScratch::default())
    }

    /// [`Regex::find_utf16`] over buffers a previous search handed back.
    #[must_use]
    pub fn find_utf16_with<'r, 't>(
        &'r self,
        text: &'t [u16],
        start: usize,
        config: ExecConfig,
        scratch: MatchScratch,
    ) -> Matches<'r, 't> {
        self.find_units(text, start, config, scratch)
    }

    /// [`Regex::find_latin1`] over buffers a previous search handed back.
    #[must_use]
    pub fn find_latin1_with<'r, 't>(
        &'r self,
        text: &'t [u8],
        start: usize,
        config: ExecConfig,
        scratch: MatchScratch,
    ) -> Matches<'r, 't, u8> {
        self.find_units(text, start, config, scratch)
    }

    /// One leftmost search for the first match at or after `start`, over
    /// `scratch` in place: whether a match exists, with its group offsets
    /// written into `out` (see [`crate::NO_OFFSET`]), and the backtrack
    /// steps explored. `anchored` admits only a match beginning at `start`,
    /// as [`Matches::anchored`] does. A miss leaves `out` unspecified.
    #[inline]
    pub fn first_offsets_into<U: Unit>(
        &self,
        text: &[U],
        start: usize,
        anchored: bool,
        config: ExecConfig,
        scratch: &mut MatchScratch,
        out: &mut Vec<u32>,
    ) -> (Result<bool, ExecError>, u64) {
        let mut matches = self.find_units(text, start, config, std::mem::take(scratch));
        matches.anchored = anchored;
        let found = matches.first_offsets(out);
        let steps = matches.steps_executed;
        *scratch = matches.into_scratch();
        (found, steps)
    }

    #[inline]
    fn find_units<'r, 't, U: Unit>(
        &'r self,
        text: &'t [U],
        start: usize,
        config: ExecConfig,
        scratch: MatchScratch,
    ) -> Matches<'r, 't, U> {
        let literal = self
            .program
            .required_literal
            .as_ref()
            .filter(|lit| lit.is_complete());
        let execution = if let Some(literal) = literal {
            Execution::Literal(literal)
        } else if let Some(code) = self.native.code::<U>(&self.program) {
            code.prepare();
            Execution::Native(code)
        } else {
            Execution::Interpreted
        };
        Matches {
            regex: self,
            text,
            next_start: search_start(text, start, self.program.unicode),
            config,
            anchored: false,
            done: false,
            steps_executed: 0,
            scratch: scratch.0.unwrap_or_default(),
            execution,
        }
    }

    /// A search pinned to one tier: the interpreter (`code: None`) or the
    /// given machine code of this regex's program.
    #[cfg(test)]
    pub(crate) fn find_on_tier<'r, 't, U: Unit>(
        &'r self,
        text: &'t [U],
        start: usize,
        config: ExecConfig,
        code: Option<&'r NativeCode>,
    ) -> Matches<'r, 't, U> {
        if let Some(code) = code {
            code.prepare();
        }
        Matches {
            regex: self,
            text,
            next_start: search_start(text, start, self.program.unicode),
            config,
            anchored: false,
            done: false,
            steps_executed: 0,
            scratch: Box::default(),
            execution: code.map_or(Execution::Interpreted, Execution::Native),
        }
    }

    /// The lowered program.
    #[cfg(test)]
    pub(crate) fn program(&self) -> &Program {
        &self.program
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
}

/// How the leftmost search advances between candidate start positions.
///
/// Resolved once from the compiled program, so the per-candidate path is a
/// single dispatch rather than a chain of option tests.
enum Scan<'p> {
    /// Only the search's own position is a candidate: an anchored search.
    Exact,
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

/// One authoritative execution plan, selected before acquiring search scratch.
/// Explicit test-tier searches exclude literal completion to keep an oracle.
#[derive(Debug, Clone, Copy)]
enum Execution<'r> {
    Literal(&'r RequiredLiteral),
    Native(&'r NativeCode),
    Interpreted,
}

/// Lending search over successive matches of a [`Regex`] in a subject of UTF-16
/// units or Latin-1 bytes.
///
/// Each next result borrows matcher scratch. Collection owns offsets before
/// the host allocates or reenters. Resource exhaustion stays an explicit error.
#[derive(Debug)]
pub struct Matches<'r, 't, U: Unit = u16> {
    regex: &'r Regex,
    text: &'t [U],
    next_start: usize,
    config: ExecConfig,
    /// Every match begins exactly where the search stands.
    anchored: bool,
    done: bool,
    steps_executed: u64,
    scratch: Box<Buffers>,
    /// The complete scan, compiled matcher or interpreted matcher for this search.
    execution: Execution<'r>,
}

impl<U: Unit> Matches<'_, '_, U> {
    /// Restrict every match to begin exactly where the search stands, as a
    /// sticky (`y`) RegExp requires: each result tries that one candidate,
    /// and the first failing candidate ends the search without scanning
    /// ahead for a later start.
    #[must_use]
    pub fn anchored(mut self, anchored: bool) -> Self {
        self.anchored = anchored;
        self
    }

    /// Whether a match may begin at `pos`: within the subject, nowhere a
    /// start anchor rules out, and under a prefilter at a code point that
    /// can begin one. A prefilter exists only for a pattern that cannot
    /// match the empty string, so the subject's end never qualifies then.
    fn may_start_at(&self, pos: usize) -> bool {
        let program = &self.regex.program;
        if pos > self.text.len() || (program.start_anchored && pos > 0) {
            return false;
        }
        program.prefilter.as_ref().is_none_or(|pf| {
            pos < self.text.len() && pf.cp_may_start(decode_at(self.text, pos, program.unicode).0)
        })
    }

    /// Number of backtrack points explored by this search so far.
    #[must_use]
    pub fn steps_executed(&self) -> u64 {
        self.steps_executed
    }

    /// Collect at most `max_matches` remaining matches into one capture arena.
    pub fn collect_batch(&mut self, mut max_matches: usize) -> Result<MatchBatch, ExecError> {
        let regex = self.regex;
        let mut batch = MatchBatch::new(regex.program.capture_slot_count());
        while max_matches > 0 {
            let Some(found) = self.advance() else { break };
            found?;
            batch.push(self.current_slots(), &regex.program.names);
            max_matches -= 1;
        }
        Ok(batch)
    }

    /// Collect only the next match, retaining no names or arena on a miss.
    #[inline]
    pub fn collect_first(&mut self) -> Result<Option<MatchRecord>, ExecError> {
        let regex = self.regex;
        self.advance()
            .map(|found| {
                found?;
                Ok(MatchRecord::new(self.current_slots(), &regex.program.names))
            })
            .transpose()
    }

    /// Find only the next match and write its group offsets into `out`
    /// (see [`crate::NO_OFFSET`]), allocating nothing once `out` has room:
    /// `Ok(false)` on a miss, which leaves `out` unspecified.
    #[inline]
    pub fn first_offsets(&mut self, out: &mut Vec<u32>) -> Result<bool, ExecError> {
        match self.advance() {
            None => Ok(false),
            Some(found) => {
                found?;
                self.current_slots().write_offsets(out);
                Ok(true)
            }
        }
    }

    /// End the search and hand its matcher buffers back for the next one.
    #[must_use]
    pub fn into_scratch(self) -> MatchScratch {
        if self.scratch.interpreter.is_oversized() || self.scratch.native.is_oversized() {
            MatchScratch::default()
        } else {
            MatchScratch(Some(self.scratch))
        }
    }

    /// The next match by the program's machine code: the leftmost search
    /// picks candidate windows exactly as the interpreter's loop picks
    /// candidates, and the code runs each window.
    #[inline(never)]
    fn advance_native(&mut self, code: &NativeCode) -> Option<Result<(), ExecError>> {
        let program = &self.regex.program;
        let len = self.text.len();
        let mut pos = self.next_start;
        loop {
            let window = if self.anchored {
                if !self.may_start_at(pos) {
                    break;
                }
                Window {
                    start: pos,
                    end: pos,
                    filter: false,
                }
            } else if program.start_anchored {
                if pos > 0 {
                    break;
                }
                if let Some(pf) = &program.prefilter
                    && !(pos < len && pf.cp_may_start(self.text[pos].into()))
                {
                    break;
                }
                Window {
                    start: 0,
                    end: 0,
                    filter: false,
                }
            } else if let Some(lit) = &program.required_literal {
                // Every start from the earliest the occurrence admits up to
                // the last one it admits; later starts need a later
                // occurrence.
                let Some(at) = lit.find(self.text, pos.saturating_add(lit.min_offset)) else {
                    break;
                };
                pos = pos.max(at - lit.max_offset.min(at));
                Window {
                    start: pos,
                    end: at - lit.min_offset,
                    filter: false,
                }
            } else if let Some(pf) = &program.prefilter {
                // The vectorized scans find the first candidate; the code's
                // start filter, which admits exactly the same units, finds
                // the rest. A single-unit scan never tries the subject's end,
                // so neither does its window.
                if let Some(unit) = pf.single() {
                    match U::narrow(unit)
                        .and_then(|unit| self.text.get(pos..).and_then(|t| U::position(t, unit)))
                    {
                        Some(offset) => pos += offset,
                        None => break,
                    }
                    Window {
                        start: pos,
                        end: len - 1,
                        filter: true,
                    }
                } else if let (Some(bytes), Some(few)) = (U::as_latin1(self.text), pf.few_latin1())
                {
                    let rest = bytes.get(pos..).unwrap_or(&[]);
                    let found = match *few {
                        [a] => memchr::memchr(a, rest),
                        [a, b] => memchr::memchr2(a, b, rest),
                        [a, b, c] => memchr::memchr3(a, b, c, rest),
                        _ => unreachable!("a prefilter holds one to three bytes"),
                    };
                    pos = found.map_or(pos.max(len), |offset| pos + offset);
                    Window {
                        start: pos,
                        end: len,
                        filter: true,
                    }
                } else {
                    Window {
                        start: pos,
                        end: len,
                        filter: true,
                    }
                }
            } else {
                Window {
                    start: pos,
                    end: len,
                    filter: false,
                }
            };
            if window.start > len {
                break;
            }
            let remaining = self.config.step_limit.saturating_sub(self.steps_executed);
            let mut budget = remaining;
            let outcome = native::search(
                code,
                program,
                self.text,
                window,
                &mut budget,
                &mut self.scratch.native,
            );
            self.steps_executed = self.steps_executed.saturating_add(remaining - budget);
            match outcome {
                Outcome::Match => {
                    let range = self.current_slots().range();
                    let (start, end) = (range.start, range.end);
                    self.next_start = if end > start { end } else { start + 1 };
                    return Some(Ok(()));
                }
                Outcome::Exhausted(_) if self.anchored => break,
                Outcome::Exhausted(next) => pos = next,
                // The interpreter counts the step that overran the budget.
                Outcome::StepLimit => {
                    self.steps_executed = self.steps_executed.saturating_add(1);
                    self.done = true;
                    return Some(Err(ExecError::StepLimitExceeded));
                }
            }
            if pos > len {
                break;
            }
        }
        self.done = true;
        None
    }
}

impl<U: Unit> Matches<'_, '_, U> {
    /// Advance one match. Its borrowed capture view expires before the next call.
    #[allow(
        clippy::should_implement_trait,
        reason = "Each result borrows the search until its next attempt."
    )]
    pub fn next(&mut self) -> Option<Result<Match<'_>, ExecError>> {
        match self.advance()? {
            Ok(()) => Some(Ok(Match::from_slots(
                &self.regex.program.names,
                self.current_slots(),
            ))),
            Err(error) => Some(Err(error)),
        }
    }

    #[inline]
    fn current_slots(&self) -> Slots<'_> {
        let count = self.regex.program.capture_slot_count();
        match self.execution {
            Execution::Literal(_) => Slots::Encoded(&self.scratch.literal),
            Execution::Native(_) => Slots::Encoded(&self.scratch.native.captures()[..count]),
            Execution::Interpreted => {
                Slots::Interpreted(&self.scratch.interpreter.captures()[..count])
            }
        }
    }

    fn advance(&mut self) -> Option<Result<(), ExecError>> {
        if self.done {
            return None;
        }
        match self.execution {
            Execution::Literal(literal) => self.advance_literal(literal),
            Execution::Native(code) => self.advance_native(code),
            Execution::Interpreted => self.advance_interpreted(),
        }
    }
}

impl<U: Unit> Matches<'_, '_, U> {
    /// The next match by the interpreter.
    #[inline(never)]
    fn advance_interpreted(&mut self) -> Option<Result<(), ExecError>> {
        let program = &self.regex.program;
        let unicode = program.unicode;
        let input = Input::new(self.text, unicode);
        let mut pos = self.next_start;
        // Resolve the scan strategy once per call rather than re-testing it at
        // every candidate position.
        let scan = if self.anchored {
            Scan::Exact
        } else if program.start_anchored {
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
                Scan::Exact => {
                    if !self.may_start_at(pos) {
                        self.done = true;
                        return None;
                    }
                }
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
                    // The one candidate must still begin with a code point
                    // that can start a match; a prefilter exists only for a
                    // pattern that cannot match the empty string, so an empty
                    // subject fails too.
                    if let Some(pf) = &program.prefilter {
                        let starts = pos < self.text.len()
                            && pf.cp_may_start(decode_at(self.text, pos, unicode).0);
                        if !starts {
                            self.done = true;
                            return None;
                        }
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
                                pos = search_start(self.text, earliest, unicode);
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
                            self.text.get(pos..).and_then(|t| U::position(t, unit))
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
                    } else if let (Some(bytes), Some(few)) =
                        (U::as_latin1(self.text), pf.few_latin1())
                    {
                        // A one-byte subject and at most three bytes that can
                        // start a match: a vectorized search for any of them.
                        let rest = bytes.get(pos..).unwrap_or(&[]);
                        let found = match *few {
                            [a] => memchr::memchr(a, rest),
                            [a, b] => memchr::memchr2(a, b, rest),
                            [a, b, c] => memchr::memchr3(a, b, c, rest),
                            _ => unreachable!("a prefilter holds one to three bytes"),
                        };
                        // No further candidate: like the loop below, stop at
                        // the end, never moving back from past it.
                        pos = found.map_or(pos.max(self.text.len()), |offset| pos + offset);
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
                &mut self.scratch.interpreter,
            );
            self.steps_executed = self.steps_executed.saturating_add(steps);
            match attempt {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(true) => {
                    let end = self.current_slots().range().end;
                    self.next_start = if end > pos {
                        end
                    } else {
                        advance_scan(self.text, pos, unicode)
                    };
                    return Some(Ok(()));
                }
                Ok(false) if self.anchored => {
                    self.done = true;
                    return None;
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

/// The candidate a search from `start` tries first. Under Unicode semantics
/// an offset between the halves of a surrogate pair names the character the
/// pair encodes, so the search begins at the pair's lead.
fn search_start<U: Unit>(text: &[U], start: usize, unicode: bool) -> usize {
    if unicode
        && start > 0
        && start < text.len()
        && (0xDC00..=0xDFFF).contains(&text[start].widen())
        && (0xD800..=0xDBFF).contains(&text[start - 1].widen())
    {
        start - 1
    } else {
        start
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
                .find_utf16(&text, 0, ExecConfig::default())
                .next()
                .is_some();
            assert_eq!(m, expect, "{pat}");
        }
    }
}

#[cfg(test)]
#[path = "api/buffer_tests.rs"]
mod buffer_tests;

#[cfg(test)]
#[path = "api/anchored_tests.rs"]
mod anchored_tests;
