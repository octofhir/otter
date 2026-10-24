//! Matcher execution, owned capture batches and exact VM work accounting.
//!
//! # Contents
//! - Pattern compilation and width-preserving first/all searches.
//! - [`MatchOffsets`] — one match's group offsets in the interpreter's
//!   reused buffer, for the direct `exec`/`test` path.
//! - Sticky searches anchored at their start, one candidate per match.
//! - [`for_each_match`] — successive matches visited in place, without
//!   owning any capture storage.
//! - Scratch handoff and backtrack work transferred to the VM ledger.
//!
//! # Invariants
//! - Every returned range is owned before the subject borrow ends.
//! - Scratch is returned on success and resource exhaustion, before reentry.
//! - The regex engine never depends on VM or collector state.
//!
//! # See also
//! - [`super::RegexSearch`] owns isolate-local scratch and work limits.

pub use otter_regex::{ExecError, Match, MatchBatch, MatchRecord, NO_OFFSET, Regex};

/// One match's group offsets: `[start, end)` per group, group zero first,
/// [`NO_OFFSET`] for both ends of a group that did not participate. The buffer
/// is the interpreter's, lent to one match and handed back when it is used.
pub(crate) struct MatchOffsets(pub(crate) Vec<u32>);

impl MatchOffsets {
    /// The whole match.
    #[inline]
    pub(crate) fn range(&self) -> std::ops::Range<usize> {
        self.0[0] as usize..self.0[1] as usize
    }

    /// Groups, group zero included.
    #[inline]
    pub(crate) fn groups(&self) -> usize {
        self.0.len() / 2
    }

    /// Group `index`, `None` when it did not participate.
    #[inline]
    pub(crate) fn group(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let start = self.0[2 * index];
        (start != NO_OFFSET).then(|| start as usize..self.0[2 * index + 1] as usize)
    }

    /// Capture groups, group zero excluded.
    pub(crate) fn captures(&self) -> impl Iterator<Item = Option<std::ops::Range<usize>>> + '_ {
        (1..self.groups()).map(|index| self.group(index))
    }
}

/// One matcher result plus the exact work charged to the VM ledger.
pub(crate) struct Execution<T> {
    pub(crate) result: Result<T, ExecError>,
    pub(crate) steps: u64,
}

/// Compile a pattern from exact UTF-16 units under the engine-relevant flags.
/// `g`/`y`/`d` are spec state above the matcher and are not passed here.
pub(crate) fn compile(
    pattern_utf16: &[u16],
    ignore_case: bool,
    multiline: bool,
    dot_all: bool,
    unicode: bool,
    unicode_sets: bool,
) -> Result<Regex, String> {
    let flags = otter_regex::Flags {
        ignore_case,
        multiline,
        dot_all,
        unicode,
        unicode_sets,
    };
    // Compile straight from code units: a lone surrogate in the
    // pattern is a legal non-`u` literal (§22.2.1) and must not be
    // smoothed to U+FFFD by a UTF-8 round-trip.
    Regex::compile_utf16(pattern_utf16, flags).map_err(|e| format!("{e}"))
}

/// Collect at most `max_matches` successful matches from `start` with exact work accounting,
/// exploring at most the search's step limit of backtrack points. The
/// subject is searched at its stored width; a sticky search admits only
/// matches that begin where the previous one ended.
pub(crate) fn find_batch(
    re: &Regex,
    text: crate::string::CodeUnits<'_>,
    start: usize,
    sticky: bool,
    max_matches: usize,
    search: &super::RegexSearch<'_>,
) -> Execution<MatchBatch> {
    let config = otter_regex::ExecConfig {
        step_limit: search.step_limit,
    };
    let scratch = search.scratch.take();
    match text {
        crate::string::CodeUnits::Latin1(bytes) => collect_batch(
            re.find_latin1_with(bytes, start, config, scratch)
                .anchored(sticky),
            max_matches,
            search,
        ),
        crate::string::CodeUnits::Utf16(units) => collect_batch(
            re.find_utf16_with(units, start, config, scratch)
                .anchored(sticky),
            max_matches,
            search,
        ),
    }
}

fn collect_batch<U: otter_regex::Unit>(
    mut matches: otter_regex::Matches<'_, '_, U>,
    max_matches: usize,
    search: &super::RegexSearch<'_>,
) -> Execution<MatchBatch> {
    let result = matches.collect_batch(max_matches);
    let steps = matches.steps_executed();
    search.scratch.set(matches.into_scratch());
    Execution { result, steps }
}

/// Visit at most `max_matches` successive matches from `start`, in order,
/// with exact work accounting; `visit` returns whether to continue. Each
/// match borrows the matcher's scratch until `visit` returns, so nothing is
/// collected: a caller keeps only the ranges it needs. The result counts the
/// visited matches.
pub(crate) fn for_each_match(
    re: &Regex,
    text: crate::string::CodeUnits<'_>,
    start: usize,
    sticky: bool,
    max_matches: usize,
    search: &super::RegexSearch<'_>,
    visit: impl FnMut(&Match<'_>) -> bool,
) -> Execution<usize> {
    let config = otter_regex::ExecConfig {
        step_limit: search.step_limit,
    };
    let scratch = search.scratch.take();
    match text {
        crate::string::CodeUnits::Latin1(bytes) => visit_matches(
            re.find_latin1_with(bytes, start, config, scratch)
                .anchored(sticky),
            max_matches,
            search,
            visit,
        ),
        crate::string::CodeUnits::Utf16(units) => visit_matches(
            re.find_utf16_with(units, start, config, scratch)
                .anchored(sticky),
            max_matches,
            search,
            visit,
        ),
    }
}

fn visit_matches<U: otter_regex::Unit>(
    mut matches: otter_regex::Matches<'_, '_, U>,
    max_matches: usize,
    search: &super::RegexSearch<'_>,
    mut visit: impl FnMut(&Match<'_>) -> bool,
) -> Execution<usize> {
    let mut visited = 0;
    let mut result = Ok(());
    while visited < max_matches {
        match matches.next() {
            None => break,
            Some(Err(error)) => {
                result = Err(error);
                break;
            }
            Some(Ok(found)) => {
                visited += 1;
                if !visit(&found) {
                    break;
                }
            }
        }
    }
    let steps = matches.steps_executed();
    search.scratch.set(matches.into_scratch());
    Execution {
        result: result.map(|()| visited),
        steps,
    }
}

/// Find only the first match from `start` — the single-match path for
/// `exec`/`test`. The iterator is lazy, so this computes one match instead
/// of the whole remaining set; a sticky search tries `start` alone.
/// Exceeding the step limit remains an explicit resource error for the VM
/// boundary to surface.
pub(crate) fn find_one(
    re: &Regex,
    text: crate::string::CodeUnits<'_>,
    start: usize,
    sticky: bool,
    search: &super::RegexSearch<'_>,
) -> Execution<Option<MatchRecord>> {
    let config = otter_regex::ExecConfig {
        step_limit: search.step_limit,
    };
    let scratch = search.scratch.take();
    match text {
        crate::string::CodeUnits::Latin1(bytes) => first_match(
            re.find_latin1_with(bytes, start, config, scratch)
                .anchored(sticky),
            search,
        ),
        crate::string::CodeUnits::Utf16(units) => first_match(
            re.find_utf16_with(units, start, config, scratch)
                .anchored(sticky),
            search,
        ),
    }
}

/// [`find_one`] writing the match's offsets into `out` instead of building a
/// record: `Ok(true)` on a hit.
pub(crate) fn find_first_offsets(
    re: &Regex,
    text: crate::string::CodeUnits<'_>,
    start: usize,
    sticky: bool,
    search: &super::RegexSearch<'_>,
    out: &mut Vec<u32>,
) -> Execution<bool> {
    let config = otter_regex::ExecConfig {
        step_limit: search.step_limit,
    };
    let scratch = search.scratch.take();
    match text {
        crate::string::CodeUnits::Latin1(bytes) => first_offsets(
            re.find_latin1_with(bytes, start, config, scratch)
                .anchored(sticky),
            search,
            out,
        ),
        crate::string::CodeUnits::Utf16(units) => first_offsets(
            re.find_utf16_with(units, start, config, scratch)
                .anchored(sticky),
            search,
            out,
        ),
    }
}

fn first_offsets<U: otter_regex::Unit>(
    mut matches: otter_regex::Matches<'_, '_, U>,
    search: &super::RegexSearch<'_>,
    out: &mut Vec<u32>,
) -> Execution<bool> {
    let result = matches.first_offsets(out);
    let steps = matches.steps_executed();
    search.scratch.set(matches.into_scratch());
    Execution { result, steps }
}

fn first_match<U: otter_regex::Unit>(
    mut matches: otter_regex::Matches<'_, '_, U>,
    search: &super::RegexSearch<'_>,
) -> Execution<Option<MatchRecord>> {
    let result = matches.collect_first();
    let steps = matches.steps_executed();
    search.scratch.set(matches.into_scratch());
    Execution { result, steps }
}
