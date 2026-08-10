//! The internal document type subset: declarations, and the expansion they
//! drive.
//!
//! # Contents
//! - [`Dtd`] — what the declarations said: entities, attribute defaults.
//! - [`Context`] — the declarations plus the state one document's expansions
//!   share: which entities are open, and how much text is left to unfold.
//! - The `<!DOCTYPE …>` reader, written as methods of the scanner so it reads
//!   the document through the same primitives as the rest of the grammar.
//!
//! # Invariants
//! - No external entity and no external subset is read. A declaration that
//!   names one is remembered only for what it changes about well-formedness:
//!   once the document may have declarations this parser has not seen, a
//!   reference to an entity it never saw declared is left as written instead
//!   of being an error — unless the document declared `standalone="yes"`, in
//!   which case it promised there is nothing outside to read.
//! - Replacement text is held as UTF-16 code units, whatever the document's
//!   own encoding, because a character reference may name a character that
//!   encoding cannot spell.
//! - A general entity's replacement text keeps the general-entity references
//!   inside it as written; they are expanded where the entity is used, which
//!   is what makes recursive entities detectable rather than infinite.
//! - Every expansion is charged against one budget for the whole document and
//!   against a nesting cap, so a document that unfolds exponentially fails
//!   with an error rather than by exhausting memory.
//!
//! # See also
//! - <https://www.w3.org/TR/2008/REC-xml-20081126/#dt-doctype>
//! - [`crate::scan`] — the scanner these methods extend.

use std::collections::HashMap;

use crate::chars::{is_name_char, is_pubid_char, is_whitespace};
use crate::encoding::{Encoding, Unit, push_utf16};
use crate::error::{Error, ErrorKind, Result};
use crate::scan::Scanner;

/// How deeply entity references may nest inside one another.
pub(crate) const MAX_ENTITY_DEPTH: usize = 40;

/// How many code units all of one document's expansions may produce together.
pub(crate) const EXPANSION_BUDGET: usize = 8 << 20;

/// How deeply the groups of one content model may nest.
const MAX_CONTENT_MODEL_DEPTH: usize = 256;

/// A general entity, as its declaration described it.
pub(crate) enum Entity {
    /// Replacement text declared in the document itself.
    Internal(Vec<u16>),
    /// Text stored outside the document, which this parser does not read.
    External,
    /// Binary data named by a notation, which no reference may expand.
    Unparsed,
}

/// The declared type of an attribute, in the only distinction a
/// non-validating processor acts on: `CDATA` keeps the value as written,
/// every other type has its spaces collapsed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum AttKind {
    /// `CDATA`.
    Cdata,
    /// Any of the tokenized or enumerated types.
    Tokenized,
}

/// What a declaration says an absent attribute means.
enum AttDefault {
    /// `#REQUIRED` or `#IMPLIED`: nothing is supplied when it is absent.
    None,
    /// A default value, from either a plain default or `#FIXED`.
    Value(Vec<u16>),
}

/// One attribute's declaration for one element type.
pub(crate) struct AttDef {
    /// The attribute's name, for matching what the tag actually carried.
    name: String,
    /// The same name as code units, for handing to a sink.
    name_units: Vec<u16>,
    kind: AttKind,
    default: AttDefault,
}

impl AttDef {
    /// The name a sink should see.
    pub(crate) fn name_units(&self) -> &[u16] {
        &self.name_units
    }

    /// The value to supply for an absent attribute, if any.
    pub(crate) fn default_units(&self) -> Option<&[u16]> {
        match &self.default {
            AttDefault::None => None,
            AttDefault::Value(units) => Some(units),
        }
    }
}

/// What the document type declaration told the parser.
#[derive(Default)]
pub(crate) struct Dtd {
    general: HashMap<String, Entity>,
    parameters: HashMap<String, Vec<u16>>,
    attributes: HashMap<String, Vec<AttDef>>,
    /// Whether declarations exist that this parser did not read: an external
    /// subset, or a parameter entity stored outside the document.
    unread_declarations: bool,
    /// Whether the XML declaration said `standalone="yes"`.
    standalone: bool,
}

impl Dtd {
    /// Whether any attribute was declared at all.
    ///
    /// Asked before an element's name is built as text, so a document without
    /// declarations — which is almost every document — pays nothing for the
    /// declarations it does not have.
    pub(crate) fn declares_attributes(&self) -> bool {
        !self.attributes.is_empty()
    }

    /// The declared attributes of an element type, if any were declared.
    pub(crate) fn attributes_of(&self, element: &str) -> Option<&[AttDef]> {
        self.attributes.get(element).map(Vec::as_slice)
    }

    /// The declared type of one attribute of one element type.
    pub(crate) fn attribute_kind(&self, element: &str, attribute: &str) -> AttKind {
        self.attributes
            .get(element)
            .and_then(|defs| defs.iter().find(|def| def.name == attribute))
            .map_or(AttKind::Cdata, |def| def.kind)
    }

    /// Whether a reference to an entity no declaration named may be left as
    /// written instead of failing.
    ///
    /// A document that promised `standalone="yes"` has no declarations
    /// elsewhere to appeal to, so the reference is an error. Otherwise the
    /// declaration may be in the part of the subset this parser did not read,
    /// and the specification requires a non-validating processor not to treat
    /// that as an error.
    fn may_leave_undeclared(&self) -> bool {
        self.unread_declarations && !self.standalone
    }
}

/// The declarations plus the state every expansion in one document shares.
pub(crate) struct Context {
    /// What the declarations said.
    pub(crate) dtd: Dtd,
    /// The entities being expanded right now, outermost first, which is what
    /// makes a self-referential entity detectable.
    open: Vec<String>,
    /// How many more code units this document may unfold.
    budget: usize,
}

impl Context {
    /// A context for a document that has declared nothing yet.
    pub(crate) fn new() -> Self {
        Self {
            dtd: Dtd::default(),
            open: Vec::new(),
            budget: EXPANSION_BUDGET,
        }
    }

    /// Record what the XML declaration said about standing alone.
    pub(crate) fn set_standalone(&mut self, standalone: bool) {
        self.dtd.standalone = standalone;
    }

    /// Charge `units` of produced text against the document's budget.
    fn charge(&mut self, units: usize, at: usize) -> Result<()> {
        if units > self.budget {
            return Err(Error::new(ErrorKind::EntityExpansionLimit, at));
        }
        self.budget -= units;
        Ok(())
    }

    /// Start expanding `name`, refusing a cycle and a nest too deep.
    pub(crate) fn enter(&mut self, name: &str, at: usize) -> Result<()> {
        if self.open.iter().any(|open| open == name) {
            return Err(Error::new(ErrorKind::RecursiveEntity(name.to_owned()), at));
        }
        if self.open.len() >= MAX_ENTITY_DEPTH {
            return Err(Error::new(ErrorKind::EntityExpansionLimit, at));
        }
        self.open.push(name.to_owned());
        Ok(())
    }

    /// Finish expanding the entity [`Self::enter`] started.
    pub(crate) fn leave(&mut self) {
        self.open.pop();
    }
}

/// What a general entity reference resolved to.
pub(crate) enum Resolved {
    /// Replacement text to include, already expanded of everything but the
    /// markup it may contain.
    Text(Vec<u16>),
    /// Replacement text that contains markup, so it has to be parsed rather
    /// than included as characters.
    Markup(Vec<u16>),
    /// Nothing to include: the reference stands for text this parser does not
    /// read, and the specification lets it be left as written.
    Skipped,
}

/// The five entities every XML processor knows without a declaration.
fn predefined(name: &str) -> Option<u32> {
    match name {
        "lt" => Some(0x3C),
        "gt" => Some(0x3E),
        "amp" => Some(0x26),
        "apos" => Some(0x27),
        "quot" => Some(0x22),
        _ => None,
    }
}

/// Expand a general entity for use in an attribute value.
///
/// Attribute values hold characters and nothing else, so markup in the
/// replacement text is an error rather than something to parse, and every
/// white-space character becomes a space as the specification's attribute
/// normalization requires.
///
/// # Errors
/// Returns the first way the replacement text is not usable there.
pub(crate) fn expand_in_attribute(
    ctx: &mut Context,
    name: &str,
    out: &mut Vec<u16>,
    at: usize,
) -> Result<()> {
    let text = match ctx.dtd.general.get(name) {
        Some(Entity::Internal(text)) => text.clone(),
        Some(Entity::External) => {
            return Err(Error::new(
                ErrorKind::ExternalEntityInAttribute(name.to_owned()),
                at,
            ));
        }
        Some(Entity::Unparsed) => {
            return Err(Error::new(
                ErrorKind::UnparsedEntityReference(name.to_owned()),
                at,
            ));
        }
        None => {
            if ctx.dtd.may_leave_undeclared() {
                return Ok(());
            }
            return Err(Error::new(ErrorKind::UnknownEntity(name.to_owned()), at));
        }
    };
    ctx.enter(name, at)?;
    let result = expand_attribute_text(ctx, &text, out, at);
    ctx.leave();
    result
}

/// Include `text` in an attribute value, expanding what it refers to.
fn expand_attribute_text(
    ctx: &mut Context,
    text: &[u16],
    out: &mut Vec<u16>,
    at: usize,
) -> Result<()> {
    let mut walk = Walk::new(text);
    while let Some(code) = walk.next_scalar(at)? {
        match code {
            0x3C => {
                return Err(Error::new(
                    ErrorKind::Expected("no `<` in an attribute value"),
                    at,
                ));
            }
            0x26 => match walk.reference(at)? {
                Reference::Character(code) => {
                    ctx.charge(2, at)?;
                    push_utf16(code, out);
                }
                Reference::Entity(name) => match predefined(&name) {
                    Some(code) => {
                        ctx.charge(1, at)?;
                        push_utf16(code, out);
                    }
                    None => expand_in_attribute(ctx, &name, out, at)?,
                },
            },
            0x9 | 0xA | 0xD | 0x20 => {
                ctx.charge(1, at)?;
                out.push(0x20);
            }
            _ => {
                ctx.charge(2, at)?;
                push_utf16(code, out);
            }
        }
    }
    Ok(())
}

/// Resolve a general entity for use in content.
///
/// Replacement text that holds only characters is returned expanded, so the
/// text run it lands in stays one run. Replacement text that holds markup is
/// returned unexpanded for the scanner to parse as content.
///
/// # Errors
/// Returns the first way the reference or the replacement text is not
/// well-formed.
pub(crate) fn resolve_in_content(ctx: &mut Context, name: &str, at: usize) -> Result<Resolved> {
    let text = match ctx.dtd.general.get(name) {
        Some(Entity::Internal(text)) => text.clone(),
        // An external parsed entity is text this parser does not read; the
        // specification lets a non-validating processor skip it.
        Some(Entity::External) => return Ok(Resolved::Skipped),
        Some(Entity::Unparsed) => {
            return Err(Error::new(
                ErrorKind::UnparsedEntityReference(name.to_owned()),
                at,
            ));
        }
        None => {
            if ctx.dtd.may_leave_undeclared() {
                return Ok(Resolved::Skipped);
            }
            return Err(Error::new(ErrorKind::UnknownEntity(name.to_owned()), at));
        }
    };
    if text.contains(&0x3C) {
        // The `<` may still be inside a nested entity's text rather than this
        // one's, but the scanner handles both by parsing.
        ctx.charge(text.len(), at)?;
        return Ok(Resolved::Markup(text));
    }
    ctx.enter(name, at)?;
    let mut out = Vec::new();
    let result = expand_content_text(ctx, &text, &mut out, at);
    ctx.leave();
    match result? {
        Contains::Characters => Ok(Resolved::Text(out)),
        Contains::Markup => Ok(Resolved::Markup(text)),
    }
}

/// Whether expansion stayed characters or ran into markup.
enum Contains {
    /// Characters only; `out` holds them.
    Characters,
    /// Markup appeared, so `out` is to be discarded and the text parsed.
    Markup,
}

/// Include `text` in content, expanding what it refers to, and reporting
/// markup rather than trying to make characters of it.
fn expand_content_text(
    ctx: &mut Context,
    text: &[u16],
    out: &mut Vec<u16>,
    at: usize,
) -> Result<Contains> {
    let mut walk = Walk::new(text);
    while let Some(code) = walk.next_scalar(at)? {
        match code {
            0x3C => return Ok(Contains::Markup),
            0x26 => match walk.reference(at)? {
                Reference::Character(code) => {
                    ctx.charge(2, at)?;
                    push_utf16(code, out);
                }
                Reference::Entity(name) => match predefined(&name) {
                    Some(code) => {
                        ctx.charge(1, at)?;
                        push_utf16(code, out);
                    }
                    None => match resolve_in_content(ctx, &name, at)? {
                        Resolved::Text(text) => {
                            ctx.charge(text.len(), at)?;
                            out.extend_from_slice(&text);
                        }
                        Resolved::Markup(_) => return Ok(Contains::Markup),
                        Resolved::Skipped => {}
                    },
                },
            },
            _ => {
                ctx.charge(2, at)?;
                push_utf16(code, out);
            }
        }
    }
    Ok(Contains::Characters)
}

/// A reference found inside replacement text.
enum Reference {
    /// `&#…;`
    Character(u32),
    /// `&name;`
    Entity(String),
}

/// A cursor over UTF-16 replacement text.
struct Walk<'a> {
    units: &'a [u16],
    pos: usize,
}

impl<'a> Walk<'a> {
    fn new(units: &'a [u16]) -> Self {
        Self { units, pos: 0 }
    }

    /// The next scalar value, joining a surrogate pair.
    fn next_scalar(&mut self, at: usize) -> Result<Option<u32>> {
        let Some(&lead) = self.units.get(self.pos) else {
            return Ok(None);
        };
        self.pos += 1;
        let lead = u32::from(lead);
        if !(0xD800..=0xDBFF).contains(&lead) {
            return Ok(Some(lead));
        }
        let Some(&trail) = self.units.get(self.pos) else {
            return Err(Error::new(ErrorKind::MalformedEncoding, at));
        };
        self.pos += 1;
        Ok(Some(
            0x1_0000 + ((lead - 0xD800) << 10) + (u32::from(trail) - 0xDC00),
        ))
    }

    /// The reference whose `&` was just taken.
    fn reference(&mut self, at: usize) -> Result<Reference> {
        let bad = || Error::new(ErrorKind::BadCharacterReference, at);
        if self.units.get(self.pos) == Some(&0x23) {
            self.pos += 1;
            let hex = self.units.get(self.pos) == Some(&0x78);
            if hex {
                self.pos += 1;
            }
            let radix = if hex { 16 } else { 10 };
            let start = self.pos;
            let mut code: u32 = 0;
            while let Some(digit) = self
                .units
                .get(self.pos)
                .and_then(|&unit| char::from_u32(u32::from(unit)))
                .and_then(|digit| digit.to_digit(radix))
            {
                code = code.saturating_mul(radix).saturating_add(digit);
                self.pos += 1;
            }
            if self.pos == start || self.units.get(self.pos) != Some(&0x3B) {
                return Err(bad());
            }
            self.pos += 1;
            if !crate::chars::is_char(code) {
                return Err(bad());
            }
            return Ok(Reference::Character(code));
        }
        let start = self.pos;
        while let Some(&unit) = self.units.get(self.pos) {
            if unit == 0x3B {
                break;
            }
            self.pos += 1;
        }
        if self.units.get(self.pos) != Some(&0x3B) {
            return Err(Error::new(
                ErrorKind::Expected("`;` after an entity reference"),
                at,
            ));
        }
        let name = String::from_utf16_lossy(&self.units[start..self.pos]);
        self.pos += 1;
        if name.is_empty() {
            return Err(Error::new(ErrorKind::ExpectedName, at));
        }
        Ok(Reference::Entity(name))
    }
}

impl<E: Encoding> Scanner<'_, E> {
    /// `<!DOCTYPE name ExternalID? ('[' intSubset ']' S?)? '>'`
    pub(crate) fn scan_doctype(&mut self, ctx: &mut Context) -> Result<()> {
        self.pos += 9;
        self.require_whitespace()?;
        self.scan_name()?;
        let had_space = self.skip_whitespace();
        if self.starts_with(self.pos, b"SYSTEM") || self.starts_with(self.pos, b"PUBLIC") {
            if !had_space {
                return self.err(ErrorKind::ExpectedWhitespace);
            }
            self.scan_external_id()?;
            ctx.dtd.unread_declarations = true;
            self.skip_whitespace();
        }
        if self.at(self.pos, b'[') {
            self.pos += 1;
            self.scan_subset(ctx, true)?;
            self.expect(b"]", "`]` to end the internal subset")?;
            self.skip_whitespace();
        }
        self.expect(b">", "`>` to end the document type declaration")
    }

    /// `SYSTEM S SystemLiteral | PUBLIC S PubidLiteral S SystemLiteral`
    fn scan_external_id(&mut self) -> Result<()> {
        let public = self.starts_with(self.pos, b"PUBLIC");
        if !public && !self.starts_with(self.pos, b"SYSTEM") {
            return self.err(ErrorKind::BadDoctype("expected SYSTEM or PUBLIC"));
        }
        self.pos += 6;
        self.require_whitespace()?;
        if public {
            self.scan_pubid_literal()?;
            self.require_whitespace()?;
        }
        self.scan_quoted_literal()?;
        Ok(())
    }

    /// A public identifier, whose characters the grammar restricts to a set
    /// narrower than the rest of a document's.
    fn scan_pubid_literal(&mut self) -> Result<()> {
        let quote = match self.unit(self.pos) {
            Some(unit) if unit.is(b'"') => b'"',
            Some(unit) if unit.is(b'\'') => b'\'',
            _ => return self.err(ErrorKind::Expected("a quoted public identifier")),
        };
        self.pos += 1;
        loop {
            let Some(unit) = self.unit(self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            if unit.is(quote) {
                self.pos += 1;
                return Ok(());
            }
            let value = unit.value();
            // An apostrophe is a public identifier character, but not inside
            // an apostrophe-quoted one, where it would end the literal.
            if !is_pubid_char(value) || (quote == b'\'' && value == 0x27) {
                return self.err(ErrorKind::BadDoctype(
                    "a public identifier holds a character the grammar excludes",
                ));
            }
            self.pos += 1;
        }
    }

    /// A quoted literal of a declaration, whose text this parser does not use.
    fn scan_quoted_literal(&mut self) -> Result<()> {
        let quote = match self.unit(self.pos) {
            Some(unit) if unit.is(b'"') => b'"',
            Some(unit) if unit.is(b'\'') => b'\'',
            _ => return self.err(ErrorKind::Expected("a quoted literal")),
        };
        self.pos += 1;
        while !self.at(self.pos, quote) {
            if self.pos >= self.len() {
                return self.err(ErrorKind::UnexpectedEof);
            }
            self.pos += 1;
        }
        self.pos += 1;
        Ok(())
    }

    /// The declarations of a subset, up to `]` when `bracketed`, else up to
    /// the end of the text.
    pub(crate) fn scan_subset(&mut self, ctx: &mut Context, bracketed: bool) -> Result<()> {
        loop {
            self.skip_whitespace();
            if bracketed && self.at(self.pos, b']') {
                return Ok(());
            }
            if self.pos >= self.len() {
                if bracketed {
                    return self.err(ErrorKind::UnexpectedEof);
                }
                return Ok(());
            }
            if self.starts_with(self.pos, b"<!--") {
                self.scan_comment()?;
            } else if self.starts_with(self.pos, b"<?") {
                self.scan_processing_instruction()?;
            } else if self.starts_with(self.pos, b"<!ENTITY") {
                self.scan_entity_declaration(ctx)?;
            } else if self.starts_with(self.pos, b"<!ATTLIST") {
                self.scan_attlist_declaration(ctx)?;
            } else if self.starts_with(self.pos, b"<!ELEMENT") {
                // What an element declaration says constrains validity only,
                // and this processor does not validate; how it is written is
                // still a matter of well-formedness, so it is parsed in full
                // and then dropped.
                self.scan_element_declaration()?;
            } else if self.starts_with(self.pos, b"<!NOTATION") {
                self.scan_notation_declaration()?;
            } else if self.at(self.pos, b'%') {
                self.scan_parameter_reference(ctx)?;
            } else {
                return self.err(ErrorKind::BadDoctype("expected a markup declaration"));
            }
        }
    }

    /// A parameter-entity reference where a markup declaration may start. Its
    /// replacement text is a run of declarations, so it is read as a subset of
    /// its own.
    fn scan_parameter_reference(&mut self, ctx: &mut Context) -> Result<()> {
        let at = self.pos;
        self.pos += 1;
        let name = self.scan_name()?;
        let name = self.text_of(name);
        self.expect(b";", "`;` after a parameter-entity reference")?;
        let Some(text) = ctx.dtd.parameters.get(&name).cloned() else {
            if ctx.dtd.may_leave_undeclared() {
                return Ok(());
            }
            return Err(Error::new(ErrorKind::UnknownEntity(name), at));
        };
        ctx.charge(text.len(), at)?;
        // Reading a parameter entity at all is enough to stop an undeclared
        // general entity from being a well-formedness error: the declaration
        // could have come from a subset this parser does not read, and no
        // processor is required to tell the difference.
        ctx.dtd.unread_declarations = true;
        ctx.enter(&name, at)?;
        let result = Scanner::<crate::encoding::Utf16>::over(&text)
            .and_then(|mut nested| nested.scan_subset(ctx, false));
        ctx.leave();
        result
    }

    /// `<!ENTITY '%'? name (EntityValue | ExternalID NDATA?) '>'`
    fn scan_entity_declaration(&mut self, ctx: &mut Context) -> Result<()> {
        self.pos += 8;
        self.require_whitespace()?;
        let parameter = self.at(self.pos, b'%');
        if parameter {
            self.pos += 1;
            self.require_whitespace()?;
        }
        let name = self.scan_name()?;
        let name = self.text_of(name);
        self.require_whitespace()?;
        if self.at(self.pos, b'"') || self.at(self.pos, b'\'') {
            let text = self.scan_entity_value(ctx)?;
            // The first declaration of a name is the one that counts; a later
            // one is not an error and is not used.
            if parameter {
                ctx.dtd.parameters.entry(name).or_insert(text);
            } else {
                ctx.dtd
                    .general
                    .entry(name)
                    .or_insert(Entity::Internal(text));
            }
        } else {
            self.scan_external_id()?;
            let mut unparsed = false;
            let had_space = self.skip_whitespace();
            if self.starts_with(self.pos, b"NDATA") {
                if !had_space {
                    return self.err(ErrorKind::ExpectedWhitespace);
                }
                self.pos += 5;
                self.require_whitespace()?;
                self.scan_name()?;
                unparsed = true;
            }
            if parameter {
                if unparsed {
                    return self.err(ErrorKind::BadDoctype(
                        "a parameter entity may not name unparsed data",
                    ));
                }
                // Declarations this parser will never see, which is what
                // makes an undeclared reference something to leave alone
                // rather than to reject.
                ctx.dtd.unread_declarations = true;
            } else {
                let entity = if unparsed {
                    Entity::Unparsed
                } else {
                    Entity::External
                };
                ctx.dtd.general.entry(name).or_insert(entity);
            }
        }
        self.skip_whitespace();
        self.expect(b">", "`>` to end an entity declaration")
    }

    /// The quoted replacement text of an internal entity.
    ///
    /// Character references and line ends are resolved here, as the
    /// specification requires of a literal entity value; general-entity
    /// references are kept as written, to be expanded where the entity is
    /// used.
    fn scan_entity_value(&mut self, ctx: &mut Context) -> Result<Vec<u16>> {
        let quote = if self.at(self.pos, b'"') { b'"' } else { b'\'' };
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let at = self.pos;
            let Some(unit) = self.unit(self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            if unit.is(quote) {
                self.pos += 1;
                return Ok(out);
            }
            match unit.value() {
                0x26 if self.at(self.pos + 1, b'#') => {
                    let code = self.scan_character_reference()?;
                    ctx.charge(2, at)?;
                    push_utf16(code, &mut out);
                }
                0x26 => {
                    // A general-entity reference is bypassed: it is copied as
                    // written and expanded where the entity is used.
                    self.pos += 1;
                    let name = self.scan_name()?;
                    self.expect(b";", "`;` after an entity reference")?;
                    ctx.charge(name.len() + 2, at)?;
                    out.push(0x26);
                    self.push_span(&name, &mut out)?;
                    out.push(0x3B);
                }
                0x25 => {
                    return self.err(ErrorKind::BadDoctype(
                        "a parameter-entity reference may not appear inside a declaration",
                    ));
                }
                0xD => {
                    ctx.charge(1, at)?;
                    out.push(0x0A);
                    self.pos += if self.at(self.pos + 1, b'\n') { 2 } else { 1 };
                }
                _ => {
                    let (code, width) = self.scalar(self.pos)?;
                    ctx.charge(2, at)?;
                    push_utf16(code, &mut out);
                    self.pos += width;
                }
            }
        }
    }

    /// `<!ATTLIST element (name type default)* '>'`
    fn scan_attlist_declaration(&mut self, ctx: &mut Context) -> Result<()> {
        self.pos += 9;
        self.require_whitespace()?;
        let element = self.scan_name()?;
        let element = self.text_of(element);
        loop {
            let had_space = self.skip_whitespace();
            if self.at(self.pos, b'>') {
                self.pos += 1;
                return Ok(());
            }
            if !had_space {
                return self.err(ErrorKind::ExpectedWhitespace);
            }
            let name = self.scan_name()?;
            let name = self.text_of(name);
            self.require_whitespace()?;
            let kind = self.scan_attribute_type()?;
            self.require_whitespace()?;
            let default = self.scan_attribute_default(ctx)?;
            let mut name_units = Vec::new();
            for code in name.chars() {
                push_utf16(code as u32, &mut name_units);
            }
            let defs = ctx.dtd.attributes.entry(element.clone()).or_default();
            if !defs.iter().any(|def| def.name == name) {
                defs.push(AttDef {
                    name,
                    name_units,
                    kind,
                    default,
                });
            }
        }
    }

    /// `CDATA | ID | IDREF | IDREFS | ENTITY | ENTITIES | NMTOKEN | NMTOKENS |
    /// NOTATION S '(' Name ('|' Name)* ')' | '(' Nmtoken ('|' Nmtoken)* ')'`
    fn scan_attribute_type(&mut self) -> Result<AttKind> {
        if self.starts_with(self.pos, b"CDATA") {
            self.pos += 5;
            return Ok(AttKind::Cdata);
        }
        if self.at(self.pos, b'(') {
            self.scan_alternatives(false)?;
            return Ok(AttKind::Tokenized);
        }
        if self.starts_with(self.pos, b"NOTATION") {
            self.pos += 8;
            self.require_whitespace()?;
            self.scan_alternatives(true)?;
            return Ok(AttKind::Tokenized);
        }
        for name in [
            &b"IDREFS"[..],
            b"IDREF",
            b"ID",
            b"ENTITIES",
            b"ENTITY",
            b"NMTOKENS",
            b"NMTOKEN",
        ] {
            if self.starts_with(self.pos, name) {
                self.pos += name.len();
                return Ok(AttKind::Tokenized);
            }
        }
        self.err(ErrorKind::BadDoctype("expected an attribute type"))
    }

    /// The parenthesized alternatives of an enumeration, whose members are
    /// name tokens, or of a notation type, whose members are names.
    fn scan_alternatives(&mut self, names: bool) -> Result<()> {
        self.expect(b"(", "`(`")?;
        loop {
            self.skip_whitespace();
            if names {
                self.scan_name()?;
            } else {
                self.scan_nmtoken()?;
            }
            self.skip_whitespace();
            if self.at(self.pos, b'|') {
                self.pos += 1;
                continue;
            }
            return self.expect(b")", "`)` to end the list of allowed values");
        }
    }

    /// A `Nmtoken`: name characters, without the restriction on the first.
    fn scan_nmtoken(&mut self) -> Result<()> {
        let start = self.pos;
        while self.pos < self.len() {
            let (code, width) = self.scalar(self.pos)?;
            if !is_name_char(code) {
                break;
            }
            self.pos += width;
        }
        if self.pos == start {
            return self.err(ErrorKind::ExpectedName);
        }
        Ok(())
    }

    /// `<!ELEMENT` S Name S contentspec S? `>`
    fn scan_element_declaration(&mut self) -> Result<()> {
        self.pos += 9;
        self.require_whitespace()?;
        self.scan_name()?;
        self.require_whitespace()?;
        self.scan_content_spec()?;
        self.skip_whitespace();
        self.expect(b">", "`>` to end an element declaration")
    }

    /// `EMPTY | ANY | Mixed | children`
    fn scan_content_spec(&mut self) -> Result<()> {
        if self.starts_with(self.pos, b"EMPTY") {
            self.pos += 5;
            return Ok(());
        }
        if self.starts_with(self.pos, b"ANY") {
            self.pos += 3;
            return Ok(());
        }
        if !self.at(self.pos, b'(') {
            return self.err(ErrorKind::BadDoctype("expected a content model"));
        }
        let mut probe = self.pos + 1;
        while self
            .unit(probe)
            .is_some_and(|unit| is_whitespace(unit.value()))
        {
            probe += 1;
        }
        if self.starts_with(probe, b"#PCDATA") {
            return self.scan_mixed();
        }
        self.scan_particle_group(0)?;
        self.scan_occurrence();
        Ok(())
    }

    /// `'(' S? '#PCDATA' (S? '|' S? Name)* S? ')*' | '(' S? '#PCDATA' S? ')'`
    fn scan_mixed(&mut self) -> Result<()> {
        self.expect(b"(", "`(`")?;
        self.skip_whitespace();
        self.expect(b"#PCDATA", "`#PCDATA`")?;
        let mut named = false;
        loop {
            self.skip_whitespace();
            if !self.at(self.pos, b'|') {
                break;
            }
            self.pos += 1;
            self.skip_whitespace();
            self.scan_name()?;
            named = true;
        }
        self.expect(b")", "`)` to end a mixed content model")?;
        if named {
            // Naming element types alongside `#PCDATA` makes the repetition
            // obligatory, not optional.
            return self.expect(b"*", "`*` after a mixed content model");
        }
        if self.at(self.pos, b'*') {
            self.pos += 1;
        }
        Ok(())
    }

    /// `choice | seq`: alternatives or a sequence, never both in one group.
    fn scan_particle_group(&mut self, depth: usize) -> Result<()> {
        if depth >= MAX_CONTENT_MODEL_DEPTH {
            return self.err(ErrorKind::DepthLimit);
        }
        self.expect(b"(", "`(`")?;
        self.skip_whitespace();
        self.scan_particle(depth)?;
        let mut separator: Option<u8> = None;
        loop {
            self.skip_whitespace();
            let next = match self.unit(self.pos) {
                Some(unit) if unit.is(b'|') => b'|',
                Some(unit) if unit.is(b',') => b',',
                _ => break,
            };
            if *separator.get_or_insert(next) != next {
                return self.err(ErrorKind::BadDoctype(
                    "a content model mixes `,` and `|` in one group",
                ));
            }
            self.pos += 1;
            self.skip_whitespace();
            self.scan_particle(depth)?;
        }
        self.expect(b")", "`)` to end a content model")
    }

    /// `(Name | choice | seq) ('?' | '*' | '+')?`
    fn scan_particle(&mut self, depth: usize) -> Result<()> {
        if self.at(self.pos, b'(') {
            self.scan_particle_group(depth + 1)?;
        } else {
            self.scan_name()?;
        }
        self.scan_occurrence();
        Ok(())
    }

    /// The optional repetition mark that follows a particle.
    fn scan_occurrence(&mut self) {
        if self.at(self.pos, b'?') || self.at(self.pos, b'*') || self.at(self.pos, b'+') {
            self.pos += 1;
        }
    }

    /// `<!NOTATION` S Name S (ExternalID | PublicID) S? `>`
    fn scan_notation_declaration(&mut self) -> Result<()> {
        self.pos += 10;
        self.require_whitespace()?;
        self.scan_name()?;
        self.require_whitespace()?;
        if self.starts_with(self.pos, b"PUBLIC") {
            self.pos += 6;
            self.require_whitespace()?;
            self.scan_pubid_literal()?;
            // A notation may name a public identifier alone, where an
            // external identifier would go on to a system one.
            if self.skip_whitespace() && (self.at(self.pos, b'"') || self.at(self.pos, b'\'')) {
                self.scan_quoted_literal()?;
            }
        } else if self.starts_with(self.pos, b"SYSTEM") {
            self.pos += 6;
            self.require_whitespace()?;
            self.scan_quoted_literal()?;
        } else {
            return self.err(ErrorKind::BadDoctype("expected SYSTEM or PUBLIC"));
        }
        self.skip_whitespace();
        self.expect(b">", "`>` to end a notation declaration")
    }

    /// `#REQUIRED | #IMPLIED | (#FIXED S)? AttValue`
    fn scan_attribute_default(&mut self, ctx: &mut Context) -> Result<AttDefault> {
        if self.starts_with(self.pos, b"#REQUIRED") {
            self.pos += 9;
            return Ok(AttDefault::None);
        }
        if self.starts_with(self.pos, b"#IMPLIED") {
            self.pos += 8;
            return Ok(AttDefault::None);
        }
        if self.starts_with(self.pos, b"#FIXED") {
            self.pos += 6;
            self.require_whitespace()?;
        }
        Ok(AttDefault::Value(self.scan_declared_value(ctx)?))
    }

    /// A quoted attribute value in a declaration, normalized exactly as the
    /// same value written on a tag would be.
    fn scan_declared_value(&mut self, ctx: &mut Context) -> Result<Vec<u16>> {
        let quote = match self.unit(self.pos) {
            Some(unit) if unit.is(b'"') => b'"',
            Some(unit) if unit.is(b'\'') => b'\'',
            _ => return self.err(ErrorKind::Expected("a quoted attribute value")),
        };
        self.pos += 1;
        let mut out = Vec::new();
        loop {
            let at = self.pos;
            let Some(unit) = self.unit(self.pos) else {
                return self.err(ErrorKind::UnexpectedEof);
            };
            if unit.is(quote) {
                self.pos += 1;
                return Ok(out);
            }
            match unit.value() {
                0x3C => {
                    return self.err(ErrorKind::Expected("no `<` in an attribute value"));
                }
                0x26 if self.at(self.pos + 1, b'#') => {
                    let code = self.scan_character_reference()?;
                    ctx.charge(2, at)?;
                    push_utf16(code, &mut out);
                }
                0x26 => {
                    self.pos += 1;
                    let name = self.scan_name()?;
                    self.expect(b";", "`;` after an entity reference")?;
                    let name = self.text_of(name);
                    match predefined(&name) {
                        Some(code) => {
                            ctx.charge(1, at)?;
                            push_utf16(code, &mut out);
                        }
                        None => expand_in_attribute(ctx, &name, &mut out, at)?,
                    }
                }
                value if is_whitespace(value) => {
                    ctx.charge(1, at)?;
                    out.push(0x20);
                    self.pos += if value == 0xD && self.at(self.pos + 1, b'\n') {
                        2
                    } else {
                        1
                    };
                }
                _ => {
                    let (code, width) = self.scalar(self.pos)?;
                    ctx.charge(2, at)?;
                    push_utf16(code, &mut out);
                    self.pos += width;
                }
            }
        }
    }

    /// Copy the document's `span` into UTF-16 output.
    fn push_span(&self, span: &core::ops::Range<usize>, out: &mut Vec<u16>) -> Result<()> {
        let mut pos = span.start;
        while pos < span.end {
            let (code, width) = self.scalar(pos)?;
            push_utf16(code, out);
            pos += width;
        }
        Ok(())
    }
}
