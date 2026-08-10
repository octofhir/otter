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
//! - An entity whose replacement text holds markup is parsed by a scanner of
//!   its own over that text, which is what makes the specification's rule that
//!   a parsed entity must match the `content` production hold by construction:
//!   an element opened inside an entity has nowhere else to close.
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/>
//! - [`crate::index`] — where the stopping positions come from.
//! - [`crate::dtd`] — the declarations that drive expansion.

use core::ops::Range;

use crate::chars::{is_char, is_name_char, is_name_start, is_whitespace};
use crate::dtd::{self, Context, Resolved};
use crate::encoding::{Encoding, Unit, Utf16, push_utf16};
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
    let mut ctx = Context::new();
    Scanner::<E>::over(units)?.run(sink, &mut ctx)
}

/// A sink driven by a scanner over an entity's replacement text, forwarding to
/// the sink of the document that referred to the entity.
///
/// Replacement text is held as UTF-16, so its pieces reach the document's sink
/// as [`Piece::Widened`] whatever the document's own code unit is.
struct Expanded<'s, U: Unit> {
    inner: &'s mut dyn Sink<U>,
}

/// The same run of replacement text, addressed to a sink of `U`.
fn widened<U: Unit>(piece: Piece<'_, u16>) -> Piece<'_, U> {
    match piece {
        Piece::Source { units, .. } | Piece::Rewritten(units) | Piece::Widened(units) => {
            Piece::Widened(units)
        }
    }
}

impl<U: Unit> Sink<u16> for Expanded<'_, U> {
    fn start_element(&mut self, name: Piece<'_, u16>) {
        self.inner.start_element(widened(name));
    }

    fn attribute(&mut self, name: Piece<'_, u16>, value: Piece<'_, u16>) {
        self.inner.attribute(widened(name), widened(value));
    }

    fn text(&mut self, text: Piece<'_, u16>) {
        self.inner.text(widened(text));
    }

    fn end_element(&mut self) {
        self.inner.end_element();
    }
}

/// Where character data stopped.
enum Data {
    /// At markup or at the end of the text; the run has been emitted.
    Stopped,
    /// At a reference to an entity whose replacement text holds markup. The
    /// run has been emitted; the text is for the caller to parse as content.
    Entity(String, Vec<u16>),
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

pub(crate) struct Scanner<'a, E: Encoding> {
    units: &'a [E::Unit],
    index: Index,
    /// Forward-only position in the index.
    cursor: usize,
    /// Position in the document, in code units.
    pub(crate) pos: usize,
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
    /// Index `units` and open a scanner over them.
    pub(crate) fn over(units: &'a [E::Unit]) -> Result<Self> {
        let index = index::build::<E>(units)?;
        Ok(Self::new(units, index))
    }

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

    pub(crate) fn err<T>(&self, kind: ErrorKind) -> Result<T> {
        Err(Error::new(kind, self.pos))
    }

    /// How many code units the text being scanned has.
    pub(crate) fn len(&self) -> usize {
        self.units.len()
    }

    #[inline]
    pub(crate) fn unit(&self, pos: usize) -> Option<E::Unit> {
        self.units.get(pos).copied()
    }

    #[inline]
    pub(crate) fn at(&self, pos: usize, ascii: u8) -> bool {
        self.unit(pos).is_some_and(|unit| unit.is(ascii))
    }

    /// Whether the document has `literal` at `pos`.
    pub(crate) fn starts_with(&self, pos: usize, literal: &[u8]) -> bool {
        literal
            .iter()
            .enumerate()
            .all(|(offset, &byte)| self.at(pos + offset, byte))
    }

    /// Advance over `literal`, or fail saying what was wanted.
    pub(crate) fn expect(&mut self, literal: &'static [u8], what: &'static str) -> Result<()> {
        if !self.starts_with(self.pos, literal) {
            return self.err(ErrorKind::Expected(what));
        }
        self.pos += literal.len();
        Ok(())
    }

    /// Skip whitespace, reporting whether any was there.
    pub(crate) fn skip_whitespace(&mut self) -> bool {
        let start = self.pos;
        while self
            .unit(self.pos)
            .is_some_and(|unit| is_whitespace(unit.value()))
        {
            self.pos += 1;
        }
        self.pos > start
    }

    pub(crate) fn require_whitespace(&mut self) -> Result<()> {
        if self.skip_whitespace() {
            Ok(())
        } else {
            self.err(ErrorKind::ExpectedWhitespace)
        }
    }

    /// The scalar at `pos`, which the index has already proven well-formed.
    pub(crate) fn scalar(&self, pos: usize) -> Result<(u32, usize)> {
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
    pub(crate) fn scan_name(&mut self) -> Result<Range<usize>> {
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

    fn run<S: Sink<E::Unit>>(&mut self, sink: &mut S, ctx: &mut Context) -> Result<()> {
        self.scan_prolog(ctx)?;
        if !self.at(self.pos, b'<') || self.at(self.pos + 1, b'/') {
            return self.err(ErrorKind::RootElementCount);
        }
        self.scan_element_tree(sink, ctx)?;
        self.scan_trailing_misc()?;
        if self.pos < self.units.len() {
            return self.err(ErrorKind::RootElementCount);
        }
        Ok(())
    }

    fn scan_prolog(&mut self, ctx: &mut Context) -> Result<()> {
        if self.starts_with(self.pos, b"<?xml")
            && self
                .unit(self.pos + 5)
                .is_some_and(|unit| is_whitespace(unit.value()))
        {
            self.scan_xml_declaration(ctx)?;
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
                self.scan_doctype(ctx)?;
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
    fn scan_xml_declaration(&mut self, ctx: &mut Context) -> Result<()> {
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
            // A document that stands alone promises there is nothing outside
            // it to declare, which is what makes an undeclared entity an
            // error rather than something to leave as written.
            ctx.set_standalone(yes);
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

    fn scan_element_tree<S: Sink<E::Unit>>(
        &mut self,
        sink: &mut S,
        ctx: &mut Context,
    ) -> Result<()> {
        self.scan_start_tag(sink, ctx)?;
        while !self.open.is_empty() {
            if let Data::Entity(name, text) = self.scan_char_data(sink, ctx)? {
                self.scan_entity_as_content(&name, &text, sink, ctx)?;
                continue;
            }
            if self.pos >= self.units.len() {
                let name = self.open.last().cloned().unwrap_or(0..0);
                return Err(Error::new(
                    ErrorKind::UnclosedElement(self.text_of(name)),
                    self.pos,
                ));
            }
            self.scan_markup(sink, ctx)?;
        }
        Ok(())
    }

    /// The `content` production of an entity's replacement text: everything a
    /// document body may hold, ending only when the text does and with every
    /// element it opened closed again.
    fn run_entity_content<S: Sink<E::Unit>>(
        &mut self,
        sink: &mut S,
        ctx: &mut Context,
    ) -> Result<()> {
        loop {
            if let Data::Entity(name, text) = self.scan_char_data(sink, ctx)? {
                self.scan_entity_as_content(&name, &text, sink, ctx)?;
                continue;
            }
            if self.pos >= self.units.len() {
                break;
            }
            self.scan_markup(sink, ctx)?;
        }
        if let Some(name) = self.open.last().cloned() {
            return Err(Error::new(
                ErrorKind::UnclosedElement(self.text_of(name)),
                self.pos,
            ));
        }
        Ok(())
    }

    /// One construct starting at the `<` the scanner is sitting on.
    fn scan_markup<S: Sink<E::Unit>>(&mut self, sink: &mut S, ctx: &mut Context) -> Result<()> {
        match self.unit(self.pos + 1) {
            None => self.err(ErrorKind::UnexpectedEof),
            Some(unit) if unit.is(b'/') => self.scan_end_tag(sink),
            Some(unit) if unit.is(b'?') => self.scan_processing_instruction(),
            Some(unit) if unit.is(b'!') => {
                if self.starts_with(self.pos, b"<!--") {
                    self.scan_comment()
                } else if self.starts_with(self.pos, b"<![CDATA[") {
                    self.scan_cdata(sink)
                } else {
                    self.err(ErrorKind::Expected("a comment or CDATA section"))
                }
            }
            Some(_) => self.scan_start_tag(sink, ctx),
        }
    }

    /// Parse an entity's replacement text as content, driving the same sink.
    ///
    /// The nested scanner keeps its own stack of open elements, so an element
    /// the entity opens has to close inside it — the specification's rule that
    /// a parsed entity matches `content`, enforced by construction.
    fn scan_entity_as_content<S: Sink<E::Unit>>(
        &mut self,
        name: &str,
        text: &[u16],
        sink: &mut S,
        ctx: &mut Context,
    ) -> Result<()> {
        ctx.enter(name, self.pos)?;
        // The sink is held behind a trait object on purpose: an entity inside
        // an entity would otherwise wrap the wrapper, and the sink's type
        // would grow with every level of nesting.
        let mut expanded = Expanded { inner: sink };
        let result = Scanner::<Utf16>::over(text)
            .and_then(|mut nested| nested.run_entity_content(&mut expanded, ctx));
        ctx.leave();
        result
    }

    fn scan_start_tag<S: Sink<E::Unit>>(&mut self, sink: &mut S, ctx: &mut Context) -> Result<()> {
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
                self.supply_declared_attributes(&name, sink, ctx);
                self.open.push(name);
                return Ok(());
            }
            if self.starts_with(self.pos, b"/>") {
                self.pos += 2;
                self.supply_declared_attributes(&name, sink, ctx);
                sink.end_element();
                return Ok(());
            }
            if !had_space {
                return self.err(ErrorKind::ExpectedWhitespace);
            }
            self.scan_attribute(&name, sink, ctx)?;
        }
    }

    /// Give the sink the attributes the declarations supply and the tag did
    /// not write, in declaration order and after everything it did write.
    fn supply_declared_attributes<S: Sink<E::Unit>>(
        &self,
        element: &Range<usize>,
        sink: &mut S,
        ctx: &Context,
    ) {
        if !ctx.dtd.declares_attributes() {
            return;
        }
        let element = self.text_of(element.clone());
        let Some(defs) = ctx.dtd.attributes_of(&element) else {
            return;
        };
        for def in defs {
            let Some(value) = def.default_units() else {
                continue;
            };
            let declared = String::from_utf16_lossy(def.name_units());
            if self
                .attr_names
                .iter()
                .any(|written| self.text_of(written.clone()) == declared)
            {
                continue;
            }
            sink.attribute(Piece::Widened(def.name_units()), Piece::Widened(value));
        }
    }

    fn scan_attribute<S: Sink<E::Unit>>(
        &mut self,
        element: &Range<usize>,
        sink: &mut S,
        ctx: &mut Context,
    ) -> Result<()> {
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
        let (start, end) = self.scan_attribute_value(quote, ctx)?;
        // An attribute declared as anything but CDATA has its spaces
        // collapsed and trimmed, which only a declaration can tell us.
        if ctx.dtd.declares_attributes()
            && ctx
                .dtd
                .attribute_kind(&self.text_of(element.clone()), &self.text_of(name.clone()))
                == dtd::AttKind::Tokenized
        {
            self.collapse_spaces(start, end);
        }
        let units = self.units;
        let key = Piece::Source {
            units: &units[name.clone()],
            offset: name.start,
        };
        let value = self.run_piece(start, end);
        sink.attribute(key, value);
        Ok(())
    }

    /// Trim the run's leading and trailing spaces and squeeze the runs inside
    /// it, as the specification's normalization for a non-CDATA attribute
    /// requires. Every white-space character is already a space by then.
    fn collapse_spaces(&mut self, start: usize, end: usize) {
        if self.state == Scratch::Untouched {
            self.scratch.clear();
            self.scratch.extend_from_slice(&self.units[start..end]);
            self.state = Scratch::Narrow;
        }
        match self.state {
            Scratch::Wide => squeeze(&mut self.wide, 0x20),
            _ => squeeze(&mut self.scratch, E::Unit::from_ascii(0x20)),
        }
    }

    /// Scan an attribute value up to `quote`, applying both line-end and
    /// attribute-value normalization. Returns the value's span in the
    /// document, which is only meaningful when nothing was rewritten.
    fn scan_attribute_value(&mut self, quote: u8, ctx: &mut Context) -> Result<(usize, usize)> {
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
                    self.scan_reference_in_attribute(ctx)?;
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

    /// Character data up to the next `<`, to an entity that holds markup, or
    /// to the end of the text.
    fn scan_char_data<S: Sink<E::Unit>>(
        &mut self,
        sink: &mut S,
        ctx: &mut Context,
    ) -> Result<Data> {
        let start = self.pos;
        self.begin_run(start);
        let mut entity = None;
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
                let held = self.scan_reference_in_content(ctx)?;
                self.verbatim_from = self.pos;
                if let Some(held) = held {
                    // The run ends where the reference began; what the entity
                    // holds is markup and belongs to the caller to parse.
                    entity = Some(held);
                    break entry;
                }
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
        // A run that stopped at an entity was already flushed up to the `&`,
        // and the scanner has moved past the reference, so there is nothing
        // left between what was copied and where the run ends.
        if entity.is_none() {
            self.flush_if_rewritten(end)?;
        }
        self.emit_run(start, end, sink);
        match entity {
            Some((name, text)) => Ok(Data::Entity(name, text)),
            None => Ok(Data::Stopped),
        }
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

    /// A character reference whose `&` is at `self.pos`.
    pub(crate) fn scan_character_reference(&mut self) -> Result<u32> {
        self.pos += 2;
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
        Ok(code)
    }

    /// A reference inside an attribute value, appended to the run in progress.
    ///
    /// An attribute value is characters and nothing else, so an entity that
    /// holds markup fails here rather than being parsed.
    fn scan_reference_in_attribute(&mut self, ctx: &mut Context) -> Result<()> {
        let at = self.pos;
        if self.at(self.pos + 1, b'#') {
            let code = self.scan_character_reference()?;
            return self.push_scalar(code);
        }
        self.pos += 1;
        let name = self.scan_name()?;
        self.expect(b";", "`;` after an entity reference")?;
        if let Some(code) = self.predefined(&name) {
            return self.push_scalar(code);
        }
        let name = self.text_of(name);
        let mut text = Vec::new();
        dtd::expand_in_attribute(ctx, &name, &mut text, at)?;
        self.push_utf16(&text)
    }

    /// A reference in content. Returns the entity whose replacement text holds
    /// markup, which the caller parses; anything else joins the run in
    /// progress.
    fn scan_reference_in_content(
        &mut self,
        ctx: &mut Context,
    ) -> Result<Option<(String, Vec<u16>)>> {
        let at = self.pos;
        if self.at(self.pos + 1, b'#') {
            let code = self.scan_character_reference()?;
            self.push_scalar(code)?;
            return Ok(None);
        }
        self.pos += 1;
        let name = self.scan_name()?;
        self.expect(b";", "`;` after an entity reference")?;
        if let Some(code) = self.predefined(&name) {
            self.push_scalar(code)?;
            return Ok(None);
        }
        let name = self.text_of(name);
        match dtd::resolve_in_content(ctx, &name, at)? {
            Resolved::Text(text) => {
                self.push_utf16(&text)?;
                Ok(None)
            }
            Resolved::Markup(text) => Ok(Some((name, text))),
            Resolved::Skipped => Ok(None),
        }
    }

    /// The character one of the five entities every processor knows stands
    /// for, without building the name as text.
    fn predefined(&self, name: &Range<usize>) -> Option<u32> {
        let replacement = match self.units[name.clone()].len() {
            2 if self.name_is(name, b"lt") => b'<',
            2 if self.name_is(name, b"gt") => b'>',
            3 if self.name_is(name, b"amp") => b'&',
            4 if self.name_is(name, b"apos") => b'\'',
            4 if self.name_is(name, b"quot") => b'"',
            _ => return None,
        };
        Some(u32::from(replacement))
    }

    /// Append UTF-16 replacement text to the run in progress.
    fn push_utf16(&mut self, units: &[u16]) -> Result<()> {
        let mut pos = 0;
        while pos < units.len() {
            let lead = u32::from(units[pos]);
            pos += 1;
            let code = if (0xD800..=0xDBFF).contains(&lead) {
                let trail = units
                    .get(pos)
                    .copied()
                    .ok_or_else(|| Error::new(ErrorKind::MalformedEncoding, self.pos))?;
                pos += 1;
                0x1_0000 + ((lead - 0xD800) << 10) + (u32::from(trail) - 0xDC00)
            } else {
                lead
            };
            self.push_scalar(code)?;
        }
        Ok(())
    }

    fn name_is(&self, span: &Range<usize>, literal: &[u8]) -> bool {
        self.units[span.clone()]
            .iter()
            .zip(literal)
            .all(|(unit, &byte)| unit.is(byte))
    }

    // ---- discarded constructs -------------------------------------------

    pub(crate) fn scan_comment(&mut self) -> Result<()> {
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

    pub(crate) fn scan_processing_instruction(&mut self) -> Result<()> {
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

    /// A name's text, for an error message or for matching a declaration.
    pub(crate) fn text_of(&self, span: Range<usize>) -> String {
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

/// Trim `space` from both ends of `buffer` and squeeze every run of it inside
/// to one, which is what an attribute of a declared non-CDATA type gets.
fn squeeze<T: Copy + PartialEq>(buffer: &mut Vec<T>, space: T) {
    let mut written = 0;
    let mut pending = false;
    for read in 0..buffer.len() {
        let unit = buffer[read];
        if unit == space {
            pending = written > 0;
            continue;
        }
        if pending {
            buffer[written] = space;
            written += 1;
            pending = false;
        }
        buffer[written] = unit;
        written += 1;
    }
    buffer.truncate(written);
}
