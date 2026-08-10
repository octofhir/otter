//! The scanner: document text in, sink events out.
//!
//! # Contents
//! - [`parse`] — index a document and walk it, driving a [`Sink`].
//! - [`MAX_DEPTH`] — the element nesting cap.
//!
//! # Invariants
//! - Nesting is tracked on an explicit stack, never by recursion, so a deep
//!   document costs memory rather than call frames.
//! - The structural index is walked forward only. Character data, CDATA and
//!   the runs between references are located by hopping from one index entry
//!   to the next, so unremarkable content is never examined unit by unit.
//! - A run that no reference and no line end interrupted reaches the sink as
//!   [`Piece::Source`], a slice of the document; everything else is assembled
//!   in scratch that is reused for the whole parse.
//! - Line ends are normalized before anything else looks at the text, as the
//!   specification requires: `\r\n` and a lone `\r` both become `\n`.
//! - Attribute values additionally get the CDATA normalization: a literal tab,
//!   newline or carriage return becomes a space, while the same character
//!   written as a reference is kept as itself.
//! - Comments and processing instructions are validated and discarded.
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/>
//! - [`crate::index`] — where the stopping positions come from.

use core::ops::Range;

use crate::chars::{is_char, is_name_char, is_name_start, is_whitespace};
use crate::encoding::{Encoding, Unit, push_utf16};
use crate::error::{Error, ErrorKind, Result};
use crate::index::{self, Index};
use crate::sink::{Piece, Sink};

/// How deeply elements may nest.
pub const MAX_DEPTH: usize = 4096;

/// Parse `units` as a document, driving `sink`.
///
/// # Errors
/// Returns the first way in which the document is not well-formed.
pub fn parse<E: Encoding, S: Sink<E::Unit>>(units: &[E::Unit], sink: &mut S) -> Result<()> {
    let index = index::build::<E>(units)?;
    Scanner::<E>::new(units, index).run(sink)
}

/// Where the units of the run being assembled currently live.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scratch {
    /// Nothing has been rewritten; the run is still a slice of the document.
    Untouched,
    /// The run is being assembled in the document's own units.
    Narrow,
    /// The run had to be widened to UTF-16.
    Wide,
}

struct Scanner<'a, E: Encoding> {
    units: &'a [E::Unit],
    index: Index,
    /// Forward-only position in the index.
    cursor: usize,
    /// Position in the document, in code units.
    pos: usize,
    /// Assembled units of the run in progress.
    scratch: Vec<E::Unit>,
    /// The same run, once something forced it to UTF-16.
    wide: Vec<u16>,
    state: Scratch,
    /// Start of the document run not yet copied into scratch.
    verbatim_from: usize,
    /// Name spans of the elements that are open.
    open: Vec<Range<usize>>,
    /// Name spans of the attributes seen on the tag being scanned.
    attr_names: Vec<Range<usize>>,
}

impl<'a, E: Encoding> Scanner<'a, E> {
    fn new(units: &'a [E::Unit], index: Index) -> Self {
        Self {
            units,
            index,
            cursor: 0,
            pos: 0,
            scratch: Vec::new(),
            wide: Vec::new(),
            state: Scratch::Untouched,
            verbatim_from: 0,
            open: Vec::new(),
            attr_names: Vec::new(),
        }
    }

    // ---- primitives -----------------------------------------------------

    fn err<T>(&self, kind: ErrorKind) -> Result<T> {
        Err(Error::new(kind, self.pos))
    }

    #[inline]
    fn unit(&self, pos: usize) -> Option<E::Unit> {
        self.units.get(pos).copied()
    }

    #[inline]
    fn at(&self, pos: usize, ascii: u8) -> bool {
        self.unit(pos).is_some_and(|unit| unit.is(ascii))
    }

    /// Whether the document has `literal` at `pos`.
    fn starts_with(&self, pos: usize, literal: &[u8]) -> bool {
        literal
            .iter()
            .enumerate()
            .all(|(offset, &byte)| self.at(pos + offset, byte))
    }

    /// Advance over `literal`, or fail saying what was wanted.
    fn expect(&mut self, literal: &'static [u8], what: &'static str) -> Result<()> {
        if !self.starts_with(self.pos, literal) {
            return self.err(ErrorKind::Expected(what));
        }
        self.pos += literal.len();
        Ok(())
    }

    /// Skip whitespace, reporting whether any was there.
    fn skip_whitespace(&mut self) -> bool {
        let start = self.pos;
        while self
            .unit(self.pos)
            .is_some_and(|unit| is_whitespace(unit.value()))
        {
            self.pos += 1;
        }
        self.pos > start
    }

    fn require_whitespace(&mut self) -> Result<()> {
        if self.skip_whitespace() {
            Ok(())
        } else {
            self.err(ErrorKind::ExpectedWhitespace)
        }
    }

    /// The scalar at `pos`, which the index has already proven well-formed.
    fn scalar(&self, pos: usize) -> Result<(u32, usize)> {
        let unit = self
            .unit(pos)
            .ok_or(Error::new(ErrorKind::UnexpectedEof, pos))?;
        let value = unit.value();
        if value < 0x80 {
            return Ok((value, 1));
        }
        E::decode(self.units, pos)
    }

    /// Consume a `Name`, returning its span.
    fn scan_name(&mut self) -> Result<Range<usize>> {
        let start = self.pos;
        let (code, width) = self.scalar(self.pos)?;
        if !is_name_start(code) {
            return self.err(ErrorKind::ExpectedName);
        }
        self.pos += width;
        while self.pos < self.units.len() {
            let (code, width) = self.scalar(self.pos)?;
            if !is_name_char(code) {
                break;
            }
            self.pos += width;
        }
        Ok(start..self.pos)
    }

    fn same_units(&self, left: &Range<usize>, right: &Range<usize>) -> bool {
        self.units[left.clone()] == self.units[right.clone()]
    }

    // ---- run assembly ---------------------------------------------------

    fn begin_run(&mut self, start: usize) {
        self.state = Scratch::Untouched;
        self.verbatim_from = start;
        self.scratch.clear();
        self.wide.clear();
    }

    /// Copy `verbatim_from..end` into scratch, switching away from the
    /// document's storage if this is the first rewrite.
    fn flush(&mut self, end: usize) -> Result<()> {
        let pending = self.verbatim_from..end;
        match self.state {
            Scratch::Untouched => {
                self.state = Scratch::Narrow;
                self.scratch.extend_from_slice(&self.units[pending]);
            }
            Scratch::Narrow => self.scratch.extend_from_slice(&self.units[pending]),
            Scratch::Wide => {
                let mut pos = pending.start;
                while pos < pending.end {
                    let (code, width) = self.scalar(pos)?;
                    push_utf16(code, &mut self.wide);
                    pos += width;
                }
            }
        }
        self.verbatim_from = end;
        Ok(())
    }

    /// Append one scalar to the run, widening it if the document's encoding
    /// has no way to spell that character.
    fn push_scalar(&mut self, code: u32) -> Result<()> {
        if self.state == Scratch::Untouched {
            self.state = Scratch::Narrow;
        }
        if self.state == Scratch::Narrow {
            if E::encode(code, &mut self.scratch) {
                return Ok(());
            }
            self.widen()?;
        }
        push_utf16(code, &mut self.wide);
        Ok(())
    }

    /// Move what has been assembled so far into the UTF-16 buffer.
    fn widen(&mut self) -> Result<()> {
        self.wide.clear();
        let mut pos = 0;
        while pos < self.scratch.len() {
            let (code, width) = E::decode(&self.scratch, pos)?;
            push_utf16(code, &mut self.wide);
            pos += width;
        }
        self.scratch.clear();
        self.state = Scratch::Wide;
        Ok(())
    }

    /// The run from `start` to `end` as the sink should see it.
    fn run_piece(&self, start: usize, end: usize) -> Piece<'_, E::Unit> {
        match self.state {
            Scratch::Untouched => Piece::Source {
                units: &self.units[start..end],
                offset: start,
            },
            Scratch::Narrow => Piece::Rewritten(&self.scratch),
            Scratch::Wide => Piece::Widened(&self.wide),
        }
    }

    // ---- document -------------------------------------------------------

    fn run<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        self.scan_prolog()?;
        if !self.at(self.pos, b'<') || self.at(self.pos + 1, b'/') {
            return self.err(ErrorKind::RootElementCount);
        }
        self.scan_element_tree(sink)?;
        self.scan_trailing_misc()?;
        if self.pos < self.units.len() {
            return self.err(ErrorKind::RootElementCount);
        }
        Ok(())
    }

    fn scan_prolog(&mut self) -> Result<()> {
        if self.starts_with(self.pos, b"<?xml")
            && self
                .unit(self.pos + 5)
                .is_some_and(|unit| is_whitespace(unit.value()))
        {
            self.scan_xml_declaration()?;
        }
        let mut seen_doctype = false;
        loop {
            self.skip_whitespace();
            if self.starts_with(self.pos, b"<!--") {
                self.scan_comment()?;
            } else if self.starts_with(self.pos, b"<!DOCTYPE") {
                if seen_doctype {
                    return self.err(ErrorKind::Expected("a single document type declaration"));
                }
                seen_doctype = true;
                self.skip_doctype()?;
            } else if self.starts_with(self.pos, b"<?") {
                self.scan_processing_instruction()?;
            } else {
                return Ok(());
            }
        }
    }

    fn scan_trailing_misc(&mut self) -> Result<()> {
        loop {
            self.skip_whitespace();
            if self.starts_with(self.pos, b"<!--") {
                self.scan_comment()?;
            } else if self.starts_with(self.pos, b"<?") {
                self.scan_processing_instruction()?;
            } else {
                return Ok(());
            }
        }
    }

    /// `<?xml version="1.x" encoding="…"? standalone="…"? ?>`
    fn scan_xml_declaration(&mut self) -> Result<()> {
        self.pos += 5;
        self.require_whitespace()?;
        self.expect(b"version", "a version pseudo-attribute")?;
        let version = self.scan_pseudo_attribute_value()?;
        let digits = &self.units[version];
        if digits.len() < 3
            || !digits[0].is(b'1')
            || !digits[1].is(b'.')
            || !digits[2..]
                .iter()
                .all(|unit| (0x30..=0x39).contains(&unit.value()))
        {
            return self.err(ErrorKind::BadDeclaration("version must be 1.x"));
        }
        let had_space = self.skip_whitespace();
        if self.starts_with(self.pos, b"encoding") {
            if !had_space {
                return self.err(ErrorKind::ExpectedWhitespace);
            }
            self.pos += 8;
            let name = self.scan_pseudo_attribute_value()?;
            let first = self.units[name.clone()]
                .first()
                .map_or(0, |unit| unit.value());
            let legal = char::from_u32(first).is_some_and(|c| c.is_ascii_alphabetic());
            if !legal {
                return self.err(ErrorKind::BadDeclaration("encoding name is not a name"));
            }
            self.skip_whitespace();
        }
        if self.starts_with(self.pos, b"standalone") {
            self.pos += 10;
            let value = self.scan_pseudo_attribute_value()?;
            let units = &self.units[value];
            let yes =
                units.len() == 3 && units[0].is(b'y') && units[1].is(b'e') && units[2].is(b's');
            let no = units.len() == 2 && units[0].is(b'n') && units[1].is(b'o');
            if !yes && !no {
                return self.err(ErrorKind::BadDeclaration("standalone must be yes or no"));
            }
            self.skip_whitespace();
        }
        self.expect(b"?>", "`?>`")
    }

    /// ` = "value"` of a declaration pseudo-attribute, returning the span of
    /// the value.
    fn scan_pseudo_attribute_value(&mut self) -> Result<Range<usize>> {
        self.skip_whitespace();
        self.expect(b"=", "`=`")?;
        self.skip_whitespace();
        let quote = match self.unit(self.pos) {
            Some(unit) if unit.is(b'"') => b'"',
            Some(unit) if unit.is(b'\'') => b'\'',
            _ => return self.err(ErrorKind::Expected("a quoted value")),
        };
        self.pos += 1;
        let start = self.pos;
        while !self.at(self.pos, quote) {
            if self.pos >= self.units.len() {
                return self.err(ErrorKind::UnexpectedEof);
            }
            self.pos += 1;
        }
        let value = start..self.pos;
        self.pos += 1;
        Ok(value)
    }

    // ---- elements -------------------------------------------------------

    fn scan_element_tree<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        self.scan_start_tag(sink)?;
        while !self.open.is_empty() {
            self.scan_char_data(sink)?;
            if self.pos >= self.units.len() {
                let name = self.open.last().cloned().unwrap_or(0..0);
                return Err(Error::new(
                    ErrorKind::UnclosedElement(self.text_of(name)),
                    self.pos,
                ));
            }
            match self.unit(self.pos + 1) {
                None => return self.err(ErrorKind::UnexpectedEof),
                Some(unit) if unit.is(b'/') => self.scan_end_tag(sink)?,
                Some(unit) if unit.is(b'?') => self.scan_processing_instruction()?,
                Some(unit) if unit.is(b'!') => {
                    if self.starts_with(self.pos, b"<!--") {
                        self.scan_comment()?;
                    } else if self.starts_with(self.pos, b"<![CDATA[") {
                        self.scan_cdata(sink)?;
                    } else {
                        return self.err(ErrorKind::Expected("a comment or CDATA section"));
                    }
                }
                Some(_) => self.scan_start_tag(sink)?,
            }
        }
        Ok(())
    }

    fn scan_start_tag<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        self.pos += 1;
        let name = self.scan_name()?;
        if self.open.len() >= MAX_DEPTH {
            return self.err(ErrorKind::DepthLimit);
        }
        sink.start_element(Piece::Source {
            units: &self.units[name.clone()],
            offset: name.start,
        });
        self.attr_names.clear();
        loop {
            let had_space = self.skip_whitespace();
            if self.at(self.pos, b'>') {
                self.pos += 1;
                self.open.push(name);
                return Ok(());
            }
            if self.starts_with(self.pos, b"/>") {
                self.pos += 2;
                sink.end_element();
                return Ok(());
            }
            if !had_space {
                return self.err(ErrorKind::ExpectedWhitespace);
            }
            self.scan_attribute(sink)?;
        }
    }

    fn scan_attribute<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        let name = self.scan_name()?;
        if self
            .attr_names
            .iter()
            .any(|seen| self.same_units(seen, &name))
        {
            return Err(Error::new(
                ErrorKind::DuplicateAttribute(self.text_of(name)),
                self.pos,
            ));
        }
        self.attr_names.push(name.clone());
        self.skip_whitespace();
        self.expect(b"=", "`=`")?;
        self.skip_whitespace();
        let quote = match self.unit(self.pos) {
            Some(unit) if unit.is(b'"') => b'"',
            Some(unit) if unit.is(b'\'') => b'\'',
            _ => return self.err(ErrorKind::Expected("a quoted attribute value")),
        };
        self.pos += 1;
        let (start, end) = self.scan_attribute_value(quote)?;
        let units = self.units;
        let key = Piece::Source {
            units: &units[name.clone()],
            offset: name.start,
        };
        let value = self.run_piece(start, end);
        sink.attribute(key, value);
        Ok(())
    }

    /// Scan an attribute value up to `quote`, applying both line-end and
    /// attribute-value normalization. Returns the value's span in the
    /// document, which is only meaningful when nothing was rewritten.
    fn scan_attribute_value(&mut self, quote: u8) -> Result<(usize, usize)> {
        let start = self.pos;
        self.begin_run(start);
        loop {
            let Some(unit) = self.unit(self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            let value = unit.value();
            if value == u32::from(quote) {
                let end = self.pos;
                self.flush_if_rewritten(end)?;
                self.pos += 1;
                return Ok((start, end));
            }
            match value {
                0x3C => return self.err(ErrorKind::Expected("no `<` in an attribute value")),
                0x26 => {
                    self.flush(self.pos)?;
                    self.scan_reference()?;
                    self.verbatim_from = self.pos;
                }
                0x9 | 0xA | 0xD => {
                    self.flush(self.pos)?;
                    self.push_scalar(0x20)?;
                    // A carriage return and the newline it may lead are one
                    // line end, and so one space.
                    self.pos += if value == 0xD && self.at(self.pos + 1, b'\n') {
                        2
                    } else {
                        1
                    };
                    self.verbatim_from = self.pos;
                }
                _ => self.pos += 1,
            }
        }
    }

    fn scan_end_tag<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        self.pos += 2;
        let name = self.scan_name()?;
        self.skip_whitespace();
        self.expect(b">", "`>`")?;
        let Some(open) = self.open.pop() else {
            return Err(Error::new(
                ErrorKind::UnexpectedEndTag(self.text_of(name)),
                self.pos,
            ));
        };
        if !self.same_units(&open, &name) {
            return Err(Error::new(
                ErrorKind::MismatchedEndTag {
                    expected: self.text_of(open),
                    found: self.text_of(name),
                },
                self.pos,
            ));
        }
        sink.end_element();
        Ok(())
    }

    // ---- content --------------------------------------------------------

    /// Character data up to the next `<`, or to the end of the document.
    fn scan_char_data<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        let start = self.pos;
        self.begin_run(start);
        let end = loop {
            let Some(entry) = self.index.seek(&mut self.cursor, self.pos) else {
                self.pos = self.units.len();
                break self.units.len();
            };
            let unit = self.units[entry];
            if unit.is(b'<') {
                self.pos = entry;
                break entry;
            }
            if unit.is(b'&') {
                self.flush(entry)?;
                self.pos = entry;
                self.scan_reference()?;
                self.verbatim_from = self.pos;
                continue;
            }
            if unit.is(b'\r') {
                self.flush(entry)?;
                self.push_scalar(0x0A)?;
                self.pos = entry + if self.at(entry + 1, b'\n') { 2 } else { 1 };
                self.verbatim_from = self.pos;
                continue;
            }
            // `>`: legal in text unless it closes a literal `]]>`.
            if entry >= start + 2 && self.at(entry - 1, b']') && self.at(entry - 2, b']') {
                self.pos = entry;
                return self.err(ErrorKind::Expected("no literal `]]>` in character data"));
            }
            self.pos = entry + 1;
        };
        self.flush_if_rewritten(end)?;
        self.emit_run(start, end, sink);
        Ok(())
    }

    fn scan_cdata<S: Sink<E::Unit>>(&mut self, sink: &mut S) -> Result<()> {
        self.pos += 9;
        let start = self.pos;
        self.begin_run(start);
        let end = loop {
            let Some(entry) = self.index.seek(&mut self.cursor, self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            let unit = self.units[entry];
            if unit.is(b'\r') {
                self.flush(entry)?;
                self.push_scalar(0x0A)?;
                self.pos = entry + if self.at(entry + 1, b'\n') { 2 } else { 1 };
                self.verbatim_from = self.pos;
                continue;
            }
            if unit.is(b'>')
                && entry >= start + 2
                && self.at(entry - 1, b']')
                && self.at(entry - 2, b']')
            {
                break entry - 2;
            }
            self.pos = entry + 1;
        };
        self.flush_if_rewritten(end)?;
        self.emit_run(start, end, sink);
        self.pos = end + 3;
        Ok(())
    }

    fn emit_run<S: Sink<E::Unit>>(&mut self, start: usize, end: usize, sink: &mut S) {
        let piece = self.run_piece(start, end);
        if !piece.is_empty() {
            sink.text(piece);
        }
    }

    fn flush_if_rewritten(&mut self, end: usize) -> Result<()> {
        if self.state != Scratch::Untouched {
            self.flush(end)?;
        }
        Ok(())
    }

    /// A reference at `self.pos`, appended to the run in progress.
    fn scan_reference(&mut self) -> Result<()> {
        self.pos += 1;
        if self.at(self.pos, b'#') {
            self.pos += 1;
            let hex = self.at(self.pos, b'x');
            if hex {
                self.pos += 1;
            }
            let radix = if hex { 16 } else { 10 };
            let start = self.pos;
            let mut code: u32 = 0;
            let mut overflow = false;
            while let Some(digit) = self
                .unit(self.pos)
                .and_then(|unit| char::from_u32(unit.value()))
                .and_then(|c| c.to_digit(radix))
            {
                code = code.saturating_mul(radix).saturating_add(digit);
                overflow |= code > 0x10_FFFF;
                self.pos += 1;
            }
            if self.pos == start {
                return self.err(ErrorKind::BadCharacterReference);
            }
            self.expect(b";", "`;` after a character reference")?;
            if overflow || !is_char(code) {
                return self.err(ErrorKind::BadCharacterReference);
            }
            return self.push_scalar(code);
        }
        let name = self.scan_name()?;
        self.expect(b";", "`;` after an entity reference")?;
        let replacement = match self.units[name.clone()].len() {
            2 if self.name_is(&name, b"lt") => b'<',
            2 if self.name_is(&name, b"gt") => b'>',
            3 if self.name_is(&name, b"amp") => b'&',
            4 if self.name_is(&name, b"apos") => b'\'',
            4 if self.name_is(&name, b"quot") => b'"',
            _ => {
                return Err(Error::new(
                    ErrorKind::UnknownEntity(self.text_of(name)),
                    self.pos,
                ));
            }
        };
        self.push_scalar(u32::from(replacement))
    }

    fn name_is(&self, span: &Range<usize>, literal: &[u8]) -> bool {
        self.units[span.clone()]
            .iter()
            .zip(literal)
            .all(|(unit, &byte)| unit.is(byte))
    }

    // ---- discarded constructs -------------------------------------------

    fn scan_comment(&mut self) -> Result<()> {
        self.pos += 4;
        loop {
            match self.unit(self.pos) {
                None => return self.err(ErrorKind::UnexpectedEof),
                Some(unit) if unit.is(b'-') => {
                    if !self.at(self.pos + 1, b'-') {
                        self.pos += 1;
                        continue;
                    }
                    if !self.at(self.pos + 2, b'>') {
                        return self.err(ErrorKind::Expected("no `--` inside a comment"));
                    }
                    self.pos += 3;
                    return Ok(());
                }
                Some(_) => self.pos += 1,
            }
        }
    }

    fn scan_processing_instruction(&mut self) -> Result<()> {
        self.pos += 2;
        let target = self.scan_name()?;
        if self.units[target.clone()].len() == 3
            && ["xml", "xmL", "xMl", "xML", "Xml", "XmL", "XMl", "XML"]
                .iter()
                .any(|spelling| self.name_is(&target, spelling.as_bytes()))
        {
            return self.err(ErrorKind::Expected(
                "a processing-instruction target other than `xml`",
            ));
        }
        if !self.starts_with(self.pos, b"?>") {
            self.require_whitespace()?;
        }
        loop {
            if self.pos >= self.units.len() {
                return self.err(ErrorKind::UnexpectedEof);
            }
            if self.starts_with(self.pos, b"?>") {
                self.pos += 2;
                return Ok(());
            }
            self.pos += 1;
        }
    }

    /// Step over a document type declaration without interpreting it, keeping
    /// track of quoting and of the internal subset's brackets so that a `>`
    /// inside either does not end it.
    fn skip_doctype(&mut self) -> Result<()> {
        self.pos += 9;
        let mut quote: Option<u8> = None;
        let mut in_subset = false;
        loop {
            let Some(unit) = self.unit(self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            self.pos += 1;
            let value = unit.value();
            match quote {
                Some(open) if value == u32::from(open) => quote = None,
                Some(_) => {}
                None => match value {
                    0x22 => quote = Some(b'"'),
                    0x27 => quote = Some(b'\''),
                    0x5B => in_subset = true,
                    0x5D => in_subset = false,
                    0x3E if !in_subset => return Ok(()),
                    _ => {}
                },
            }
        }
    }

    /// A name's text, for an error message.
    fn text_of(&self, span: Range<usize>) -> String {
        let mut out = String::new();
        let mut pos = span.start;
        while pos < span.end {
            let Ok((code, width)) = self.scalar(pos) else {
                break;
            };
            out.push(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER));
            pos += width;
        }
        out
    }
}
