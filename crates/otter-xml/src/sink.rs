//! The event interface between the scanner and whatever builds the result.
//!
//! # Contents
//! - [`Piece`] — one run of text handed to the sink.
//! - [`Sink`] — the events a document produces.
//!
//! # Invariants
//! - Events arrive in document order, and elements nest: every
//!   [`Sink::start_element`] is matched by exactly one [`Sink::end_element`].
//! - All of an element's [`Sink::attribute`] events precede its first child
//!   event, so at `end_element` the sink knows everything about that element
//!   and can build it in one step, bottom up.
//! - A [`Piece`] borrows either the document or the scanner's scratch, and is
//!   valid only for the duration of the call it arrives in.
//! - Comments and processing instructions produce no events at all.
//!
//! # See also
//! - [`crate::scan`] — the producer.
//! - [`crate::tree`] — the sink that builds an owning tree.

use crate::encoding::Unit;

/// One run of text, in the document's own code units where it can be.
#[derive(Debug, Clone, Copy)]
pub enum Piece<'a, U: Unit> {
    /// A verbatim run of the document, starting at the given offset.
    ///
    /// Nothing had to be rewritten — no reference expanded, no line end
    /// normalized — so a sink may share the document's storage rather than
    /// copy. This is the case the structural index is shaped to produce.
    Source {
        /// The units.
        units: &'a [U],
        /// Where they start in the document.
        offset: usize,
    },
    /// A run the scanner had to rewrite, still in the document's units.
    Rewritten(&'a [U]),
    /// A run the scanner had to rewrite and widen, because a character
    /// reference named a character the document's encoding cannot spell.
    Widened(&'a [u16]),
}

impl<'a, U: Unit> Piece<'a, U> {
    /// Whether the run has no units.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        match self {
            Self::Source { units, .. } | Self::Rewritten(units) => units.is_empty(),
            Self::Widened(units) => units.is_empty(),
        }
    }

    /// The run as text, without copying, when the encoding spells it the way
    /// Rust spells `str` — the common case for a run of an ASCII document.
    ///
    /// `None` means the run must be walked scalar by scalar; see [`Self::chars`].
    #[must_use]
    pub fn as_str<E>(&self) -> Option<&'a str>
    where
        E: crate::encoding::Encoding<Unit = U>,
    {
        match *self {
            Self::Source { units, .. } | Self::Rewritten(units) => E::as_str(units),
            Self::Widened(_) => None,
        }
    }

    /// The run's scalar values, however it is stored.
    ///
    /// Sinks that keep the document's representation should match on the
    /// variant instead; this is the convenient path for sinks that do not.
    pub fn chars<E>(&self) -> Chars<'_, U>
    where
        E: crate::encoding::Encoding<Unit = U>,
    {
        match self {
            Self::Source { units, .. } | Self::Rewritten(units) => Chars::Narrow {
                units,
                pos: 0,
                decode: E::decode,
            },
            Self::Widened(units) => Chars::Wide { units, pos: 0 },
        }
    }
}

/// Scalar values of a [`Piece`], whichever storage it uses.
pub enum Chars<'a, U: Unit> {
    /// Units in the document's encoding, decoded by that encoding.
    Narrow {
        /// The units.
        units: &'a [U],
        /// How far the walk has got.
        pos: usize,
        /// The encoding's decoder.
        decode: fn(&[U], usize) -> crate::error::Result<(u32, usize)>,
    },
    /// UTF-16 units the scanner widened to.
    Wide {
        /// The units.
        units: &'a [u16],
        /// How far the walk has got.
        pos: usize,
    },
}

impl<U: Unit> Iterator for Chars<'_, U> {
    type Item = u32;

    fn next(&mut self) -> Option<u32> {
        match self {
            Self::Narrow { units, pos, decode } => {
                if *pos >= units.len() {
                    return None;
                }
                // Scratch and document alike were validated before reaching a
                // sink, so this cannot fail.
                let (code, width) = decode(units, *pos).ok()?;
                *pos += width;
                Some(code)
            }
            Self::Wide { units, pos } => {
                let lead = *units.get(*pos)? as u32;
                if !(0xD800..=0xDBFF).contains(&lead) {
                    *pos += 1;
                    return Some(lead);
                }
                let trail = *units.get(*pos + 1)? as u32;
                *pos += 2;
                Some(0x1_0000 + ((lead - 0xD800) << 10) + (trail - 0xDC00))
            }
        }
    }
}

/// Receives a document's events and decides what to build from them.
pub trait Sink<U: Unit> {
    /// An element started. Its attributes follow before any child event.
    fn start_element(&mut self, name: Piece<'_, U>);

    /// One attribute of the element that started most recently.
    fn attribute(&mut self, name: Piece<'_, U>, value: Piece<'_, U>);

    /// Character data, from either character data proper or a CDATA section.
    fn text(&mut self, text: Piece<'_, U>);

    /// The most recently started element ended.
    fn end_element(&mut self);
}
