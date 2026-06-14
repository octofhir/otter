//! The public API surface — [`Regex`], [`Match`], and the match iterator.
//!
//! This is the exact surface a host VM consumes (see `docs/regex-rewrite-
//! research.md` §1 for the contract mapped from Otter's call sites). A
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
//! - A successful scan always advances `next_start` past the current match, so
//!   the iterator terminates even on zero-width matches.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-regexp.prototype.exec> (§22.2.7.2)

use core::ops::Range;

use crate::cursor::Input;
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
        Matches {
            regex: self,
            text,
            next_start: start,
            config,
            done: false,
            scratch: backtrack::Scratch::new(),
        }
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

/// Iterator over successive matches of a [`Regex`] in a UTF-16 subject.
///
/// Mirrors the host's existing consumption: it collects `Result<Match,
/// ExecError>` and stops on the first error (the host then surfaces "no match"
/// and moves on, per the ReDoS contract).
#[derive(Debug)]
pub struct Matches<'r, 't> {
    regex: &'r Regex,
    text: &'t [u16],
    next_start: usize,
    config: ExecConfig,
    done: bool,
    scratch: backtrack::Scratch,
}

impl Iterator for Matches<'_, '_> {
    type Item = Result<Match, ExecError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let program = &self.regex.program;
        let unicode = program.unicode;
        let input = Input::new(self.text, unicode);
        let mut pos = self.next_start;
        loop {
            // First-set prefilter: skip positions that cannot start a match.
            if let Some(first) = &program.first_set {
                while pos < self.text.len() {
                    let (cp, _) = decode_at(self.text, pos, unicode);
                    if first.contains(cp) {
                        break;
                    }
                    pos = advance_scan(self.text, pos, unicode);
                }
            }
            if pos > self.text.len() {
                self.done = true;
                return None;
            }
            match backtrack::attempt(
                &self.regex.program,
                &input,
                pos,
                self.config,
                &mut self.scratch,
            ) {
                Err(e) => {
                    self.done = true;
                    return Some(Err(e));
                }
                Ok(Some(caps)) => {
                    let mat = build_match(&self.regex.program, &caps);
                    let end = mat.range.end;
                    self.next_start = if end > pos {
                        end
                    } else {
                        advance_scan(self.text, pos, unicode)
                    };
                    return Some(Ok(mat));
                }
                Ok(None) => pos = advance_scan(self.text, pos, unicode),
            }
        }
    }
}

/// Decode the code point at `pos` (which must be `< text.len()`), returning it
/// and its code-unit width. Surrogate pairs combine only in unicode mode.
fn decode_at(text: &[u16], pos: usize, unicode: bool) -> (u32, usize) {
    let hi = text[pos];
    if unicode
        && (0xD800..=0xDBFF).contains(&hi)
        && let Some(&lo) = text.get(pos + 1)
        && (0xDC00..=0xDFFF).contains(&lo)
    {
        return (
            0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00),
            2,
        );
    }
    (u32::from(hi), 1)
}

/// Advance a scan position by one code point (two units over a surrogate pair in
/// unicode mode) so the leftmost search makes progress.
fn advance_scan(text: &[u16], pos: usize, unicode: bool) -> usize {
    if unicode
        && let Some(&hi) = text.get(pos)
        && (0xD800..=0xDBFF).contains(&hi)
        && let Some(&lo) = text.get(pos + 1)
        && (0xDC00..=0xDFFF).contains(&lo)
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
    let group_names = program
        .group_names
        .iter()
        .map(|n| n.clone().unwrap_or_default())
        .collect();
    Match {
        range,
        captures,
        group_names,
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
    /// the empty string marks an unnamed group. Used by [`Match::named_groups`]
    /// and [`Match::named_group`].
    pub(crate) group_names: Vec<String>,
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

    /// Access a named group's range, deduplicating duplicate names and
    /// preferring the alternative that matched.
    #[must_use]
    pub fn named_group(&self, name: &str) -> Option<Range<usize>> {
        if name.is_empty() {
            return None;
        }
        let mut best: Option<Range<usize>> = None;
        for (idx, gname) in self.group_names.iter().enumerate() {
            if gname == name {
                let cap = self.captures.get(idx).and_then(Clone::clone);
                if cap.is_some() {
                    return cap;
                }
                if best.is_none() {
                    best = cap;
                }
            }
        }
        best
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
        assert_eq!(m.named_group("dup"), Some(0..1));
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
