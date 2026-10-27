//! §B.2.4 legacy `RegExp` static state (`RegExp.input`, `$1`…`$9`, …).
//!
//! The proposal keeps these on `%RegExp%` itself, so the state is
//! per-realm and travels with the rest of [`crate::RealmState`] across a
//! realm switch.
//!
//! # Contents
//! - [`RegExpLegacyState`] — the subject plus the ranges the last match
//!   produced.
//!
//! # Invariants
//! - Only the subject string is a GC handle; the rest is the last match's
//!   group offsets into it, copied as the matcher wrote them. A match
//!   therefore records its state with one copy into a reused buffer, and
//!   the accessors derive and slice on demand.
//! - `input` is `None` until a legacy-enabled match runs. The accessors
//!   report the empty string in that state, matching what the web
//!   depends on rather than the proposal's throw-on-empty.
//!
//! # See also
//! - <https://github.com/tc39/proposal-regexp-legacy-features>

use crate::Value;

/// Number of `$1` … `$9` capture slots the accessors expose.
pub(crate) const LEGACY_CAPTURE_SLOTS: usize = 9;

/// The realm's `%RegExp%` legacy static state.
#[derive(Debug, Clone, Default)]
pub(crate) struct RegExpLegacyState {
    /// Subject of the last legacy-enabled match, or of the last
    /// `RegExp.input` assignment. Always a string value.
    input: Option<Value>,
    /// Every group's `[start, end)` inside `input`, group zero first, the
    /// way the matcher reported them ([`NO_OFFSET`] for a group that did not
    /// participate); empty before the first match. The accessors derive
    /// `$1`…`$9`, `lastParen` and the contexts from it when read.
    offsets: Vec<u32>,
}

use crate::regexp::engine::NO_OFFSET;

impl RegExpLegacyState {
    /// Record one successful match. `captures` is the full capture list;
    /// anything past the exposed slots still counts for `lastParen`.
    pub(crate) fn record_match(
        &mut self,
        input: Value,
        matched: std::ops::Range<usize>,
        captures: impl Iterator<Item = Option<std::ops::Range<usize>>>,
    ) {
        self.input = Some(input);
        self.offsets.clear();
        self.offsets
            .extend_from_slice(&[matched.start as u32, matched.end as u32]);
        for capture in captures {
            match capture {
                Some(range) => self
                    .offsets
                    .extend_from_slice(&[range.start as u32, range.end as u32]),
                None => self.offsets.extend_from_slice(&[NO_OFFSET, NO_OFFSET]),
            }
        }
    }

    /// Record one successful match from its group offsets (see
    /// [`crate::regexp::engine::MatchOffsets`]): one copy of the buffer.
    pub(crate) fn record_offsets(
        &mut self,
        input: Value,
        offsets: &crate::regexp::engine::MatchOffsets,
    ) {
        self.input = Some(input);
        // A match records a handful of offsets: an element loop, not a
        // library copy.
        self.offsets.clear();
        self.offsets.reserve(offsets.0.len());
        for &offset in &offsets.0 {
            self.offsets.push(offset);
        }
    }

    /// Group `index` of the recorded match, `None` when it did not
    /// participate or does not exist.
    fn group(&self, index: usize) -> Option<(usize, usize)> {
        let start = *self.offsets.get(2 * index)?;
        (start != NO_OFFSET).then(|| (start as usize, self.offsets[2 * index + 1] as usize))
    }

    /// Replace the subject without disturbing the recorded ranges, per
    /// `set RegExp.input`.
    pub(crate) fn set_input(&mut self, input: Value) {
        self.input = Some(input);
    }

    /// The recorded subject, if any.
    pub(crate) fn input(&self) -> Option<Value> {
        self.input
    }

    /// Slice bounds for one named legacy property, or `None` when the
    /// property has no value yet.
    pub(crate) fn range(
        &self,
        property: LegacyRegExpProperty,
        input_len: usize,
    ) -> Option<(usize, usize)> {
        let (start, end) = self.group(0)?;
        match property {
            LegacyRegExpProperty::LastMatch => Some((start, end)),
            LegacyRegExpProperty::LeftContext => Some((0, start)),
            LegacyRegExpProperty::RightContext => Some((end, input_len.max(end))),
            LegacyRegExpProperty::LastParen => (1..self.offsets.len() / 2)
                .rev()
                .find_map(|index| self.group(index)),
            LegacyRegExpProperty::Capture(index) => {
                if usize::from(index) < LEGACY_CAPTURE_SLOTS {
                    self.group(usize::from(index) + 1)
                } else {
                    None
                }
            }
            LegacyRegExpProperty::Input => None,
        }
    }

    /// Visit the one GC handle the state owns.
    pub(crate) fn trace_roots(&self, visitor: &mut crate::gc_trace::GcRootVisitor<'_>) {
        if let Some(input) = &self.input {
            input.trace_value_slots(visitor);
        }
    }
}

/// Which legacy static an accessor reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LegacyRegExpProperty {
    /// `RegExp.input` / `RegExp.$_` — the whole subject.
    Input,
    /// `RegExp.lastMatch` / `RegExp["$&"]`.
    LastMatch,
    /// `RegExp.lastParen` / `RegExp["$+"]`.
    LastParen,
    /// ``RegExp.leftContext`` / `` RegExp["$`"] ``.
    LeftContext,
    /// `RegExp.rightContext` / `RegExp["$'"]`.
    RightContext,
    /// `RegExp.$1` … `RegExp.$9`, zero-based here.
    Capture(u8),
}

impl LegacyRegExpProperty {
    /// Compact discriminant carried as a native-accessor capture.
    pub(crate) const fn code(self) -> i32 {
        match self {
            Self::Input => 0,
            Self::LastMatch => 1,
            Self::LastParen => 2,
            Self::LeftContext => 3,
            Self::RightContext => 4,
            Self::Capture(index) => 5 + index as i32,
        }
    }

    /// Inverse of [`Self::code`].
    pub(crate) const fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Input,
            1 => Self::LastMatch,
            2 => Self::LastParen,
            3 => Self::LeftContext,
            4 => Self::RightContext,
            5..=13 => Self::Capture((code - 5) as u8),
            _ => return None,
        })
    }

    /// Resolve the accessor name the bootstrap installs.
    pub(crate) fn from_accessor_name(name: &str) -> Option<Self> {
        Some(match name {
            "input" | "$_" => Self::Input,
            "lastMatch" | "$&" => Self::LastMatch,
            "lastParen" | "$+" => Self::LastParen,
            "leftContext" | "$`" => Self::LeftContext,
            "rightContext" | "$'" => Self::RightContext,
            "$1" => Self::Capture(0),
            "$2" => Self::Capture(1),
            "$3" => Self::Capture(2),
            "$4" => Self::Capture(3),
            "$5" => Self::Capture(4),
            "$6" => Self::Capture(5),
            "$7" => Self::Capture(6),
            "$8" => Self::Capture(7),
            "$9" => Self::Capture(8),
            _ => return None,
        })
    }
}
