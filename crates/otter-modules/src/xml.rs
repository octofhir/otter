//! `Otter.XML` — XML parsing that builds JavaScript values directly.
//!
//! # Contents
//! - [`parse`] — the native behind `Otter.XML.parse`.
//! - [`otter_xml_global_installer`] — installs the namespace on the `Otter`
//!   global.
//!
//! # Invariants
//! - Values are built bottom up as the scanner reports events: when an element
//!   ends, everything about it is known and its children already exist, so it
//!   is written once rather than revised.
//! - The handle arena stays bounded. Only one handle spans the whole parse —
//!   the frame stack, an array holding the element under construction at each
//!   depth — and every event does its work in a nested scope whose handles are
//!   released as soon as it returns. Values written into the rooted frame stack
//!   stay live without a handle of their own.
//! - No intermediate tree is built: the scanner's events drive the JavaScript
//!   values directly, so a document is walked once.
//! - Text runs are accumulated in Rust, not as JavaScript strings, so an
//!   element split across several runs still yields one string.
//!
//! # See also
//! - [Handle scopes](../../../docs/site/src/content/docs/extensions/handle-scopes.md)
//! - `otter_xml::sink` — the events consumed here.

use std::borrow::Cow;

use otter_runtime::{
    OtterError, RuntimeExtensionContext, RuntimeExtensionInstaller, RuntimeLocal as Local,
    RuntimeNativeCall, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeNativeScope as NativeScope, RuntimeValue as Value, SourceInput,
};
use otter_xml::encoding::{Charset, Encoding, Latin1, Utf8, Utf16};
use otter_xml::sink::{Piece, Sink};

const NAME: &str = "Otter.XML.parse";

/// Which shape the caller asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shape {
    /// Keyed by element name, attributes as `@name`, text as `#text`.
    Compact,
    /// `{ name, attributes, children }`, document order kept exactly.
    Node,
}

/// Parse an XML document into JavaScript values.
///
/// Accepts a string, or bytes as an `ArrayBuffer` or a view over one. A string
/// is already-decoded text; bytes are decoded per their byte-order mark or
/// `encoding` declaration.
///
/// # Errors
/// A document that is not well-formed raises a `SyntaxError`; a first argument
/// that is neither text nor bytes raises a `TypeError`.
pub fn parse(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let input = scope.argument(args, 0);
        let shape = read_shape(&mut scope, args)?;

        let root = if scope.is_string(input) {
            let text = scope.string_value(input)?;
            build::<Utf8>(&mut scope, text.as_bytes(), shape)?
        } else {
            let Some(bytes) = scope.buffer_source_bytes(input) else {
                return Err(NativeError::TypeError {
                    name: NAME,
                    reason: "expected a string, an ArrayBuffer, or a view over one".to_owned(),
                });
            };
            let sniffed = otter_xml::encoding::sniff(&bytes).map_err(syntax_error)?;
            let body = &bytes[sniffed.bom_len..];
            match sniffed.charset {
                Charset::Utf8 => build::<Utf8>(&mut scope, body, shape)?,
                Charset::Latin1 => build::<Latin1>(&mut scope, body, shape)?,
                Charset::Utf16Be | Charset::Utf16Le => {
                    let units = otter_xml::encoding::decode_utf16(
                        body,
                        sniffed.charset == Charset::Utf16Be,
                    )
                    .map_err(syntax_error)?;
                    build::<Utf16>(&mut scope, &units, shape)?
                }
            }
        };
        Ok(scope.finish(root))
    })
}

fn read_shape(scope: &mut NativeScope<'_, '_>, args: &[Value]) -> Result<Shape, NativeError> {
    let options = scope.argument(args, 1);
    if !scope.is_object(options) {
        return Ok(Shape::Compact);
    }
    let compact = scope.get(options, "compact")?;
    if scope.is_undefined(compact) {
        return Ok(Shape::Compact);
    }
    if scope.boolean_value(compact).unwrap_or(true) {
        Ok(Shape::Compact)
    } else {
        Ok(Shape::Node)
    }
}

fn syntax_error(error: otter_xml::Error) -> NativeError {
    NativeError::SyntaxError {
        name: NAME,
        reason: error.to_string(),
    }
}

fn build<'s, E: Encoding>(
    scope: &mut NativeScope<'s, '_>,
    document: &[E::Unit],
    shape: Shape,
) -> Result<Local<'s>, NativeError> {
    // One handle for the whole parse: the frame stack. Everything an element
    // owns is reachable from it, so nothing else needs a handle that outlives
    // the event that made it.
    let stack = scope.array(0)?;
    let (outcome, failure, root_name) = {
        let mut builder = Builder::<E> {
            vm: Vm {
                scope,
                stack,
                failure: None,
            },
            shape,
            depth: 0,
            frames: Vec::new(),
            key: String::new(),
            root_name: String::new(),
            encoding: std::marker::PhantomData,
        };
        let outcome = otter_xml::scan::parse::<E, _>(document, &mut builder);
        (outcome, builder.vm.failure.take(), builder.root_name)
    };
    outcome.map_err(syntax_error)?;
    if let Some(failure) = failure {
        return Err(failure);
    }
    let root = scope.index(stack, 0)?;
    match shape {
        Shape::Node => Ok(root),
        Shape::Compact => {
            let wrapper = scope.object()?;
            scope.set(wrapper, &root_name, root)?;
            Ok(wrapper)
        }
    }
}

/// The scope and the one handle that spans the parse.
///
/// Kept apart from the rest of the builder so that a step can borrow the
/// scope mutably while still reading the builder's Rust-side buffers.
struct Vm<'a, 's, 'rt> {
    scope: &'a mut NativeScope<'s, 'rt>,
    stack: Local<'s>,
    failure: Option<NativeError>,
}

impl Vm<'_, '_, '_> {
    /// Run `body` in a nested handle scope, keeping the first failure.
    ///
    /// The nested scope is what bounds the arena: handles the step mints are
    /// released as it returns, while anything it stored into the frame stack
    /// stays live.
    fn step(
        &mut self,
        body: impl FnOnce(&mut NativeScope<'_, '_>, Local<'_>) -> Result<(), NativeError>,
    ) {
        if self.failure.is_some() {
            return;
        }
        let stack = self.stack;
        let result = self.scope.scope(|mut child| body(&mut child, stack));
        if let Err(error) = result {
            self.failure = Some(error);
        }
    }
}

/// What is known about one open element while its children arrive.
#[derive(Default)]
struct Frame {
    /// The element's name, needed by its parent when it closes.
    name: String,
    /// Character data seen so far, joined across runs.
    text: String,
    /// How many keys the compact object has, which decides whether the element
    /// collapses to its text.
    keys: usize,
    /// How many children the node form has appended.
    children: usize,
}

struct Builder<'a, 's, 'rt, E: Encoding> {
    vm: Vm<'a, 's, 'rt>,
    shape: Shape,
    depth: usize,
    frames: Vec<Frame>,
    /// Reused buffer for the `@name` key of an attribute.
    key: String,
    /// The root element's name, which the compact form keys its result by.
    root_name: String,
    encoding: std::marker::PhantomData<E>,
}

/// A run of text, borrowed from the document when the encoding allows it.
fn text_of<'p, E: Encoding>(piece: Piece<'p, E::Unit>) -> Cow<'p, str> {
    match piece.as_str::<E>() {
        Some(text) => Cow::Borrowed(text),
        None => Cow::Owned(piece.chars::<E>().filter_map(char::from_u32).collect()),
    }
}

fn trimmed(text: &str) -> &str {
    text.trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r'))
}

impl<E: Encoding> Sink<E::Unit> for Builder<'_, '_, '_, E> {
    fn start_element(&mut self, name: Piece<'_, E::Unit>) {
        let name = text_of::<E>(name);
        let depth = self.depth;
        if self.frames.len() <= depth {
            self.frames.push(Frame::default());
        }
        let frame = &mut self.frames[depth];
        frame.text.clear();
        frame.keys = 0;
        frame.children = 0;
        frame.name.clear();
        frame.name.push_str(&name);
        if depth == 0 {
            self.root_name.clear();
            self.root_name.push_str(&name);
        }

        let shape = self.shape;
        self.vm.step(|scope, stack| {
            let element = scope.object()?;
            if shape == Shape::Node {
                let element_name = scope.string(&name)?;
                scope.set(element, "name", element_name)?;
                let attributes = scope.object()?;
                scope.set(element, "attributes", attributes)?;
                let children = scope.array(0)?;
                scope.set(element, "children", children)?;
            }
            scope.set_index(stack, depth, element)
        });
        self.depth += 1;
    }

    fn attribute(&mut self, name: Piece<'_, E::Unit>, value: Piece<'_, E::Unit>) {
        let name = text_of::<E>(name);
        let value = text_of::<E>(value);
        let depth = self.depth - 1;
        let shape = self.shape;
        if shape == Shape::Compact {
            self.key.clear();
            self.key.push('@');
            self.key.push_str(&name);
            self.frames[depth].keys += 1;
        }
        let key: &str = if shape == Shape::Compact {
            &self.key
        } else {
            &name
        };
        self.vm.step(|scope, stack| {
            let element = scope.index(stack, depth)?;
            let text = scope.string(&value)?;
            match shape {
                Shape::Compact => scope.set(element, key, text),
                Shape::Node => {
                    let attributes = scope.get(element, "attributes")?;
                    scope.set(attributes, key, text)
                }
            }
        });
    }

    fn text(&mut self, text: Piece<'_, E::Unit>) {
        let depth = self.depth - 1;
        let run = text_of::<E>(text);
        if self.shape == Shape::Compact {
            self.frames[depth].text.push_str(&run);
            return;
        }
        let at = self.frames[depth].children;
        self.frames[depth].children += 1;
        self.vm.step(|scope, stack| {
            let element = scope.index(stack, depth)?;
            let children = scope.get(element, "children")?;
            let run = scope.string(&run)?;
            scope.set_index(children, at, run)
        });
    }

    fn end_element(&mut self) {
        self.depth -= 1;
        let depth = self.depth;
        let shape = self.shape;
        let keys = self.frames[depth].keys;
        let parent_children = if depth > 0 {
            let at = self.frames[depth - 1].children;
            self.frames[depth - 1].children += 1;
            self.frames[depth - 1].keys += 1;
            at
        } else {
            0
        };
        // The step borrows the scope mutably and these two buffers by
        // reference; they are separate fields, so both borrows stand.
        let frame = &self.frames[depth];
        let text: &str = &frame.text;
        let element_name: &str = &frame.name;

        self.vm.step(|scope, stack| {
            let element = scope.index(stack, depth)?;
            // The compact form collapses an element with no attributes and no
            // child elements to its text, and only then names its text.
            let value = if shape == Shape::Compact {
                let content = trimmed(text);
                if keys == 0 {
                    scope.string(content)?
                } else {
                    if !content.is_empty() {
                        let content = scope.string(content)?;
                        scope.set(element, "#text", content)?;
                    }
                    element
                }
            } else {
                element
            };
            if depth == 0 {
                return scope.set_index(stack, 0, value);
            }
            let parent = scope.index(stack, depth - 1)?;
            match shape {
                Shape::Node => {
                    let children = scope.get(parent, "children")?;
                    scope.set_index(children, parent_children, value)
                }
                Shape::Compact => {
                    if !scope.has_own_string_property(parent, element_name) {
                        return scope.set(parent, element_name, value);
                    }
                    let existing = scope.get(parent, element_name)?;
                    if scope.is_exact_array(existing) {
                        let at = scope.array_length(existing)?;
                        return scope.set_index(existing, at, value);
                    }
                    let repeated = scope.array(0)?;
                    scope.set_index(repeated, 0, existing)?;
                    scope.set_index(repeated, 1, value)?;
                    scope.set(parent, element_name, repeated)
                }
            }
        });
    }
}

/// Install `Otter.XML` on the global object.
#[must_use]
pub fn otter_xml_global_installer() -> RuntimeExtensionInstaller {
    RuntimeExtensionInstaller::new(install_global_xml)
}

fn install_global_xml(runtime: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    // A plain function pointer, not a closure: parsing captures nothing, and
    // a static native survives a snapshot without a factory to rebuild it.
    runtime.install_native_global_call("__otterXmlParse", 2, RuntimeNativeCall::Static(parse))?;
    runtime.install_script(SourceInput::from_javascript(
        r#"
        (function (g) {
          'use strict';
          var parse = g.__otterXmlParse;
          delete g.__otterXmlParse;
          var ns = g.Otter;
          if (ns == null || (typeof ns !== 'object' && typeof ns !== 'function')) {
            ns = {};
          }
          var xml = {};
          Object.defineProperty(xml, 'parse', {
            value: parse,
            writable: true,
            enumerable: true,
            configurable: true,
          });
          Object.defineProperty(ns, 'XML', {
            value: xml,
            writable: true,
            enumerable: true,
            configurable: true,
          });
          Object.defineProperty(g, 'Otter', {
            value: ns,
            writable: true,
            enumerable: false,
            configurable: true,
          });
        })(globalThis);
        "#,
    ))?;
    Ok(())
}
