//! XML 1.0 (Fifth Edition) parsing for Otter.
//!
//! A non-validating processor that never reads an external entity or an
//! external DTD subset, so a document can be parsed without reaching the
//! network or the filesystem. Namespaces are not resolved: prefixed names are
//! kept exactly as written.
//!
//! # Contents
//! - [`parse_bytes`] — byte input, decoded per its byte-order mark or
//!   `encoding` declaration.
//! - [`parse_utf8`], [`parse_latin1`], [`parse_utf16`] — text whose form the
//!   caller already knows, parsed in place.
//! - [`tree`] — the owning result, in both published shapes.
//! - [`sink`] — the event interface a host implements to build its own values.
//!
//! # Invariants
//! - Nothing is transcoded on the way in: the scanner is generic over the
//!   document's code unit, so text arrives at a sink in the form it was read.
//!   Byte input declaring UTF-16 is the one exception, since its byte order
//!   has to be resolved before it has code units at all.
//! - No Otter crate is a dependency. The engine consumes this parser through
//!   [`sink::Sink`]; the parser never reaches back.
//! - Every failure is fatal and carries the offset it was found at.
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/>

pub mod chars;
pub mod encoding;
pub mod error;
pub mod index;
pub mod scan;
pub mod sink;
pub mod tree;

pub use error::{Error, ErrorKind, Result};
pub use tree::{Child, Node, Value, compact};

use encoding::{Charset, Latin1, Utf8, Utf16};
use tree::TreeSink;

/// Parse a document from bytes, deciding its encoding from a byte-order mark
/// or, failing that, its `encoding` declaration.
///
/// # Errors
/// Returns the first way in which the document is not well-formed, or
/// [`ErrorKind::UnsupportedEncoding`] for an encoding this parser does not
/// read.
pub fn parse_bytes(bytes: &[u8]) -> Result<Node> {
    let sniffed = encoding::sniff(bytes)?;
    let body = &bytes[sniffed.bom_len..];
    match sniffed.charset {
        Charset::Utf8 => parse_utf8_bytes(body),
        Charset::Latin1 => parse_latin1(body),
        Charset::Utf16Be => parse_utf16(&encoding::decode_utf16(body, true)?),
        Charset::Utf16Le => parse_utf16(&encoding::decode_utf16(body, false)?),
    }
}

/// Parse already-decoded UTF-8 text.
///
/// # Errors
/// Returns the first way in which the document is not well-formed.
pub fn parse_utf8(text: &str) -> Result<Node> {
    parse_utf8_bytes(text.as_bytes())
}

fn parse_utf8_bytes(bytes: &[u8]) -> Result<Node> {
    let mut sink = TreeSink::<Utf8>::new();
    scan::parse::<Utf8, _>(bytes, &mut sink)?;
    root_of(sink.finish())
}

/// Parse text whose bytes are ISO-8859-1 code points.
///
/// # Errors
/// Returns the first way in which the document is not well-formed.
pub fn parse_latin1(bytes: &[u8]) -> Result<Node> {
    let mut sink = TreeSink::<Latin1>::new();
    scan::parse::<Latin1, _>(bytes, &mut sink)?;
    root_of(sink.finish())
}

/// Parse text already held as UTF-16 code units.
///
/// # Errors
/// Returns the first way in which the document is not well-formed.
pub fn parse_utf16(units: &[u16]) -> Result<Node> {
    let mut sink = TreeSink::<Utf16>::new();
    scan::parse::<Utf16, _>(units, &mut sink)?;
    root_of(sink.finish())
}

fn root_of(root: Option<Node>) -> Result<Node> {
    root.ok_or_else(|| Error::new(ErrorKind::RootElementCount, 0))
}
