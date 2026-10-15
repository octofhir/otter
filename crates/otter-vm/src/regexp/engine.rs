//! Matcher execution, owned capture batches and exact VM work accounting.
//!
//! # Contents
//! - Pattern compilation and width-preserving first/all searches.
//! - Sticky searches anchored at their start, one candidate per match.
//! - Scratch handoff and backtrack work transferred to the VM ledger.
//!
//! # Invariants
//! - Every returned range is owned before the subject borrow ends.
//! - Scratch is returned on success and resource exhaustion, before reentry.
//! - The regex engine never depends on VM or collector state.
//!
//! # See also
//! - [`super::RegexSearch`] owns isolate-local scratch and work limits.

pub use otter_regex::{ExecError, Match, MatchBatch, MatchRecord, Regex};

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

fn first_match<U: otter_regex::Unit>(
    mut matches: otter_regex::Matches<'_, '_, U>,
    search: &super::RegexSearch<'_>,
) -> Execution<Option<MatchRecord>> {
    let result = matches.collect_first();
    let steps = matches.steps_executed();
    search.scratch.set(matches.into_scratch());
    Execution { result, steps }
}
