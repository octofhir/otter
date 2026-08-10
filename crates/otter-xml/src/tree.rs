//! The owning tree: a sink that keeps the whole document in Rust memory.
//!
//! # Contents
//! - [`Node`] / [`Child`] — the document-order form, one node per element.
//! - [`Value`] — the compact form, keyed by element name.
//! - [`TreeSink`] — builds a [`Node`] from scanner events.
//! - [`compact`] — the compact form of a node tree.
//!
//! # Invariants
//! - Nodes are built bottom up: a node is complete when its end event
//!   arrives, so it is moved into its parent once and never revisited.
//! - The compact form loses the relative order of differently named siblings
//!   and of text between elements; the node form loses nothing.
//! - Every value in the compact form is a string. Nothing is coerced.
//!
//! # See also
//! - [`crate::sink`] — the events this consumes.

use core::marker::PhantomData;

use crate::encoding::Encoding;
use crate::sink::{Piece, Sink};

/// One element of a document.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Node {
    /// The element's name, prefix included.
    pub name: String,
    /// Its attributes, in document order.
    pub attributes: Vec<(String, String)>,
    /// Its children, in document order.
    pub children: Vec<Child>,
}

/// What an element may contain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Child {
    /// A child element.
    Element(Node),
    /// A run of character data.
    Text(String),
}

/// The compact form of a document, keyed by element name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// An element's text content, for an element with nothing else in it.
    Text(String),
    /// Attributes as `@name`, child elements by name, character data as
    /// `#text`, in first-appearance order.
    Object(Vec<(String, Value)>),
    /// Repeated children of one name, in document order.
    Array(Vec<Value>),
}

/// Builds a [`Node`] tree from scanner events.
pub struct TreeSink<E: Encoding> {
    stack: Vec<Node>,
    root: Option<Node>,
    encoding: PhantomData<E>,
}

impl<E: Encoding> Default for TreeSink<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: Encoding> TreeSink<E> {
    /// An empty sink.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            stack: Vec::new(),
            root: None,
            encoding: PhantomData,
        }
    }

    /// The document's root element, once the scan has finished.
    #[must_use]
    pub fn finish(self) -> Option<Node> {
        self.root
    }

    fn text_of(piece: Piece<'_, E::Unit>) -> String {
        piece.chars::<E>().filter_map(char::from_u32).collect()
    }
}

impl<E: Encoding> Sink<E::Unit> for TreeSink<E> {
    fn start_element(&mut self, name: Piece<'_, E::Unit>) {
        self.stack.push(Node {
            name: Self::text_of(name),
            ..Node::default()
        });
    }

    fn attribute(&mut self, name: Piece<'_, E::Unit>, value: Piece<'_, E::Unit>) {
        let pair = (Self::text_of(name), Self::text_of(value));
        if let Some(node) = self.stack.last_mut() {
            node.attributes.push(pair);
        }
    }

    fn text(&mut self, text: Piece<'_, E::Unit>) {
        let text = Self::text_of(text);
        if let Some(node) = self.stack.last_mut() {
            node.children.push(Child::Text(text));
        }
    }

    fn end_element(&mut self) {
        let Some(node) = self.stack.pop() else {
            return;
        };
        match self.stack.last_mut() {
            Some(parent) => parent.children.push(Child::Element(node)),
            None => self.root = Some(node),
        }
    }
}

/// The compact form of `root`: one key, the root element's name.
#[must_use]
pub fn compact(root: &Node) -> Value {
    Value::Object(vec![(root.name.clone(), compact_element(root))])
}

fn compact_element(node: &Node) -> Value {
    let has_elements = node
        .children
        .iter()
        .any(|child| matches!(child, Child::Element(_)));
    let text = collected_text(node);
    if node.attributes.is_empty() && !has_elements {
        return Value::Text(text);
    }

    let mut entries: Vec<(String, Value)> = Vec::with_capacity(node.attributes.len() + 2);
    for (name, value) in &node.attributes {
        entries.push((format!("@{name}"), Value::Text(value.clone())));
    }
    for child in &node.children {
        let Child::Element(element) = child else {
            continue;
        };
        let value = compact_element(element);
        match entries.iter_mut().find(|(key, _)| *key == element.name) {
            Some((_, Value::Array(items))) => items.push(value),
            Some(slot) => {
                let first = core::mem::replace(&mut slot.1, Value::Text(String::new()));
                slot.1 = Value::Array(vec![first, value]);
            }
            None => entries.push((element.name.clone(), value)),
        }
    }
    if !text.is_empty() {
        entries.push(("#text".to_owned(), Value::Text(text)));
    }
    Value::Object(entries)
}

/// An element's character data, joined and trimmed of surrounding whitespace.
fn collected_text(node: &Node) -> String {
    let mut text = String::new();
    for child in &node.children {
        if let Child::Text(run) = child {
            text.push_str(run);
        }
    }
    let trimmed = text.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'));
    if trimmed.len() == text.len() {
        text
    } else {
        trimmed.to_owned()
    }
}
