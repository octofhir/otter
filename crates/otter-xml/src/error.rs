//! Parse failures and the position they carry.
//!
//! # Contents
//! - [`ErrorKind`] — what went wrong, one variant per way a document can fail
//!   to be well-formed.
//! - [`Error`] — a kind plus the offset it was detected at.
//!
//! # Invariants
//! - `offset` counts code units of the text being parsed, not bytes of the
//!   original input, so it stays meaningful for all three encodings.
//! - Every failure is fatal: XML has no recoverable errors, and a partially
//!   built tree is never handed back.
//!
//! # See also
//! - [`crate::scan`] — the scanner that raises these.

use core::fmt;

/// What made a document fail to be well-formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    /// A code unit sequence that is not valid in the document's encoding.
    MalformedEncoding,
    /// A code point the `Char` production forbids.
    IllegalCharacter(u32),
    /// The declared encoding is not one this parser reads.
    UnsupportedEncoding(String),
    /// The document ended in the middle of a construct.
    UnexpectedEof,
    /// A construct required a name and did not get one.
    ExpectedName,
    /// Input appeared where the grammar allows only whitespace.
    ExpectedWhitespace,
    /// A literal the grammar requires was missing.
    Expected(&'static str),
    /// An end tag naming an element other than the open one.
    MismatchedEndTag {
        /// The element the end tag closed.
        expected: String,
        /// The name the end tag actually carried.
        found: String,
    },
    /// An end tag with no open element to close.
    UnexpectedEndTag(String),
    /// The document ended with elements still open.
    UnclosedElement(String),
    /// The same attribute name given twice on one element.
    DuplicateAttribute(String),
    /// Character data outside the root element.
    TextOutsideRoot,
    /// A document with no root element, or with a second one.
    RootElementCount,
    /// A reference to an entity the document never declared.
    UnknownEntity(String),
    /// An entity whose replacement text refers to itself, however indirectly.
    RecursiveEntity(String),
    /// Expansion produced more text than a document is allowed to unfold.
    EntityExpansionLimit,
    /// A reference to an entity declared with `NDATA`, which names binary data
    /// rather than text and so cannot be expanded.
    UnparsedEntityReference(String),
    /// A reference, inside an attribute value, to an entity stored outside the
    /// document. This parser reads no external entity, and the specification
    /// forbids the reference there in any case.
    ExternalEntityInAttribute(String),
    /// A document type declaration whose internal subset is malformed.
    BadDoctype(&'static str),
    /// A name that XML cannot spell, found while writing a document.
    IllegalName(String),
    /// A value whose shape is not a document, found while writing one.
    Unserializable(&'static str),
    /// A character reference that names no legal character.
    BadCharacterReference,
    /// A `<?xml …?>` declaration that is malformed or misplaced.
    BadDeclaration(&'static str),
    /// Element nesting deeper than the parser's cap.
    DepthLimit,
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedEncoding => f.write_str("malformed input for the document's encoding"),
            Self::IllegalCharacter(code) => {
                write!(
                    f,
                    "character U+{code:04X} is not allowed in an XML document"
                )
            }
            Self::UnsupportedEncoding(name) => write!(f, "unsupported encoding {name:?}"),
            Self::UnexpectedEof => f.write_str("unexpected end of document"),
            Self::ExpectedName => f.write_str("expected a name"),
            Self::ExpectedWhitespace => f.write_str("expected whitespace"),
            Self::Expected(what) => write!(f, "expected {what}"),
            Self::MismatchedEndTag { expected, found } => {
                write!(f, "expected closing tag </{expected}> but found </{found}>")
            }
            Self::UnexpectedEndTag(name) => write!(f, "closing tag </{name}> has no open element"),
            Self::UnclosedElement(name) => write!(f, "element <{name}> was never closed"),
            Self::DuplicateAttribute(name) => write!(f, "duplicate attribute {name:?}"),
            Self::TextOutsideRoot => f.write_str("character data outside the root element"),
            Self::RootElementCount => f.write_str("a document must have exactly one root element"),
            Self::UnknownEntity(name) => write!(f, "reference to undeclared entity &{name};"),
            Self::RecursiveEntity(name) => write!(f, "entity {name:?} refers to itself"),
            Self::EntityExpansionLimit => {
                f.write_str("entity expansion exceeded this parser's budget")
            }
            Self::UnparsedEntityReference(name) => {
                write!(f, "reference to unparsed entity &{name};")
            }
            Self::ExternalEntityInAttribute(name) => {
                write!(
                    f,
                    "reference to external entity &{name}; in an attribute value"
                )
            }
            Self::BadDoctype(what) => write!(f, "malformed document type declaration: {what}"),
            Self::IllegalName(name) => write!(f, "{name:?} is not a legal XML name"),
            Self::Unserializable(what) => write!(f, "cannot be written as XML: {what}"),
            Self::BadCharacterReference => f.write_str("character reference is out of range"),
            Self::BadDeclaration(what) => write!(f, "malformed XML declaration: {what}"),
            Self::DepthLimit => f.write_str("element nesting is too deep"),
        }
    }
}

/// A parse failure together with where it was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    /// What went wrong.
    pub kind: ErrorKind,
    /// Offset in code units of the text being parsed.
    pub offset: usize,
}

impl Error {
    /// Build a failure at `offset`.
    #[must_use]
    pub const fn new(kind: ErrorKind, offset: usize) -> Self {
        Self { kind, offset }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "XML Parse error: {} (at offset {})",
            self.kind, self.offset
        )
    }
}

impl core::error::Error for Error {}

/// A parse result carrying [`Error`] on failure.
pub type Result<T> = core::result::Result<T, Error>;
