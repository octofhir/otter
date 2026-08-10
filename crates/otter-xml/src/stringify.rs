//! Writing a document back out, in either published form.
//!
//! # Contents
//! - [`node`] — the document-order form.
//! - [`value`] — the compact form.
//! - [`is_name`] — whether text may be written as an element or attribute
//!   name.
//!
//! # Invariants
//! - Whatever `parse` produced, writing it and parsing the result gives the
//!   same thing back, so long as nothing is indented: text runs are escaped
//!   rather than folded, an empty element keeps its name, and a repeated name
//!   keeps its order. Indenting adds white space that is character data in the
//!   document-order form, so that form does not survive it unchanged.
//! - Text escapes `&`, `<` and `>`; an attribute value escapes those plus both
//!   quotes and the three white-space characters attribute normalization would
//!   otherwise turn into spaces. A carriage return in text is escaped too,
//!   since line ends are normalized on the way back in.
//! - A name the grammar does not allow is refused rather than written: text
//!   that cannot be read back is not output worth having.
//! - Indentation applies only where an element's content is entirely other
//!   elements, because anywhere else it would change the character data.
//!
//! # See also
//! - [`crate::tree`] — the forms written here.
//! - [`crate::scan`] — the reader whose output this reverses.

use crate::chars::{is_char, is_name_char, is_name_start};
use crate::error::{Error, ErrorKind, Result};
use crate::tree::{Child, Node, Value};

/// Write a document-order tree.
///
/// `indent` is the text one level of nesting adds; `None` writes no white
/// space of its own.
///
/// # Errors
/// Returns [`ErrorKind::IllegalName`] for a name XML cannot spell, and
/// [`ErrorKind::IllegalCharacter`] for text a document may not contain.
pub fn node(root: &Node, indent: Option<&str>) -> Result<String> {
    let mut writer = Writer::new(indent);
    writer.node(root)?;
    Ok(writer.out)
}

/// Write a compact-form document, whose root object names one element.
///
/// # Errors
/// Returns [`ErrorKind::Unserializable`] for a value that is not a document's
/// compact form, and otherwise as [`node`].
pub fn value(root: &Value, indent: Option<&str>) -> Result<String> {
    let Value::Object(entries) = root else {
        return Err(unserializable(
            "the root must be an object naming one element",
        ));
    };
    let [(name, body)] = &entries[..] else {
        return Err(unserializable(
            "the root object must name exactly one element",
        ));
    };
    if name.starts_with('@') || name == "#text" {
        return Err(unserializable(
            "the root must be an element, not text or an attribute",
        ));
    }
    let mut writer = Writer::new(indent);
    writer.compact(name, body)?;
    Ok(writer.out)
}

/// Whether `text` is a `Name`: what an element or attribute may be called.
#[must_use]
pub fn is_name(text: &str) -> bool {
    let mut chars = text.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    is_name_start(first as u32) && chars.all(|c| is_name_char(c as u32))
}

fn unserializable(what: &'static str) -> Error {
    Error::new(ErrorKind::Unserializable(what), 0)
}

struct Writer<'a> {
    out: String,
    indent: Option<&'a str>,
    depth: usize,
}

impl<'a> Writer<'a> {
    fn new(indent: Option<&'a str>) -> Self {
        Self {
            out: String::new(),
            indent: Option::filter(indent, |indent| !indent.is_empty()),
            depth: 0,
        }
    }

    /// Break the line and indent to the current depth, when indenting at all.
    fn break_line(&mut self) {
        let Some(indent) = self.indent else {
            return;
        };
        self.out.push('\n');
        for _ in 0..self.depth {
            self.out.push_str(indent);
        }
    }

    fn name(&mut self, name: &str) -> Result<()> {
        if !is_name(name) {
            return Err(Error::new(ErrorKind::IllegalName(name.to_owned()), 0));
        }
        self.out.push_str(name);
        Ok(())
    }

    fn attribute(&mut self, name: &str, value: &str) -> Result<()> {
        self.out.push(' ');
        self.name(name)?;
        self.out.push_str("=\"");
        for c in value.chars() {
            match c {
                '&' => self.out.push_str("&amp;"),
                '<' => self.out.push_str("&lt;"),
                '>' => self.out.push_str("&gt;"),
                '"' => self.out.push_str("&quot;"),
                '\'' => self.out.push_str("&apos;"),
                // Written as itself, normalization would read these back as
                // spaces.
                '\t' => self.out.push_str("&#9;"),
                '\n' => self.out.push_str("&#10;"),
                '\r' => self.out.push_str("&#13;"),
                _ => self.character(c)?,
            }
        }
        self.out.push('"');
        Ok(())
    }

    fn text(&mut self, text: &str) -> Result<()> {
        for c in text.chars() {
            match c {
                '&' => self.out.push_str("&amp;"),
                '<' => self.out.push_str("&lt;"),
                // Legal in text, except where it would close a CDATA section;
                // escaping it always is simpler and reads back the same.
                '>' => self.out.push_str("&gt;"),
                // Line ends are normalized on the way in, so a carriage
                // return only survives as a reference.
                '\r' => self.out.push_str("&#13;"),
                _ => self.character(c)?,
            }
        }
        Ok(())
    }

    fn character(&mut self, c: char) -> Result<()> {
        if !is_char(c as u32) {
            return Err(Error::new(ErrorKind::IllegalCharacter(c as u32), 0));
        }
        self.out.push(c);
        Ok(())
    }

    // ---- the document-order form ----------------------------------------

    fn node(&mut self, node: &Node) -> Result<()> {
        self.out.push('<');
        self.name(&node.name)?;
        for (name, value) in &node.attributes {
            self.attribute(name, value)?;
        }
        if node.children.is_empty() {
            self.out.push_str("/>");
            return Ok(());
        }
        self.out.push('>');
        let element_only = node
            .children
            .iter()
            .all(|child| matches!(child, Child::Element(_)));
        if element_only {
            self.depth += 1;
            for child in &node.children {
                if let Child::Element(element) = child {
                    self.break_line();
                    self.node(element)?;
                }
            }
            self.depth -= 1;
            self.break_line();
        } else {
            for child in &node.children {
                match child {
                    Child::Element(element) => self.node(element)?,
                    Child::Text(text) => self.text(text)?,
                }
            }
        }
        self.out.push_str("</");
        self.out.push_str(&node.name);
        self.out.push('>');
        Ok(())
    }

    // ---- the compact form -----------------------------------------------

    /// One element named `name`, whose value is its text or its parts.
    fn compact(&mut self, name: &str, body: &Value) -> Result<()> {
        let mut attributes: Vec<(&str, &str)> = Vec::new();
        let mut children: Vec<(&str, &Value)> = Vec::new();
        let mut text: &str = "";
        match body {
            Value::Text(run) => text = run,
            Value::Array(_) => {
                return Err(unserializable(
                    "a list must be the value of a named element",
                ));
            }
            Value::Object(entries) => {
                for (key, value) in entries {
                    if let Some(attribute) = key.strip_prefix('@') {
                        let Value::Text(run) = value else {
                            return Err(unserializable("an attribute's value must be text"));
                        };
                        attributes.push((attribute, run));
                    } else if key == "#text" {
                        let Value::Text(run) = value else {
                            return Err(unserializable("`#text` must be text"));
                        };
                        text = run;
                    } else {
                        children.push((key, value));
                    }
                }
            }
        }
        self.out.push('<');
        self.name(name)?;
        for (attribute, value) in &attributes {
            self.attribute(attribute, value)?;
        }
        if children.is_empty() && text.is_empty() {
            self.out.push_str("/>");
            return Ok(());
        }
        self.out.push('>');
        if text.is_empty() {
            self.depth += 1;
            for (key, value) in &children {
                self.compact_children(key, value)?;
            }
            self.depth -= 1;
            self.break_line();
        } else {
            // Character data settles where the element's content goes: a
            // break would become part of it.
            for (key, value) in &children {
                self.compact_repeats(key, value)?;
            }
            self.text(text)?;
        }
        self.out.push_str("</");
        self.out.push_str(name);
        self.out.push('>');
        Ok(())
    }

    /// A key's elements, each on its own line when indenting.
    fn compact_children(&mut self, key: &str, value: &Value) -> Result<()> {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.break_line();
                    self.compact(key, item)?;
                }
                Ok(())
            }
            _ => {
                self.break_line();
                self.compact(key, value)
            }
        }
    }

    /// A key's elements written without breaks, for an element whose content
    /// also holds character data.
    fn compact_repeats(&mut self, key: &str, value: &Value) -> Result<()> {
        match value {
            Value::Array(items) => {
                for item in items {
                    self.compact(key, item)?;
                }
                Ok(())
            }
            _ => self.compact(key, value),
        }
    }
}
