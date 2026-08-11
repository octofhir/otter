//! `Otter.XML` — XML parsing that builds JavaScript values directly.
//!
//! # Contents
//! - [`parse`] — the native behind `Otter.XML.parse`.
//! - [`stringify`] — the native behind `Otter.XML.stringify`.
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
//! - Writing goes the other way through one owning Rust tree, so every rule
//!   about escaping, legal names and layout lives in `otter_xml::stringify`
//!   and not in two places. Reading a JavaScript value is bounded by an
//!   explicit depth, so a cyclic value fails rather than recurses forever.
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
const WRITE: &str = "Otter.XML.stringify";

/// How deeply a value handed to [`stringify`] may nest. A cyclic value has no
/// end, so the depth is what stops it.
const MAX_VALUE_DEPTH: usize = 512;

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
/// Accepts a string, or bytes as an `ArrayBuffer`, a view over one, or a
/// `Blob`. A string is already-decoded text; bytes are decoded per their
/// byte-order mark or `encoding` declaration. A `Blob` holds its bytes in
/// memory, so they are read here rather than through the asynchronous
/// `Blob.arrayBuffer()`.
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
            // A `Blob` is bytes too, and its class declares them, so the read
            // goes through the VM's type-blind view rather than through a
            // dependency on the crate that declares the class.
            let Some(bytes) = scope
                .buffer_source_bytes(input)
                .or_else(|| scope.host_bytes(input))
            else {
                return Err(NativeError::TypeError {
                    name: NAME,
                    reason: "expected a string, an ArrayBuffer, a view over one, or a Blob"
                        .to_owned(),
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
    /// Put a fresh object in the frame stack at `depth`.
    ///
    /// Called the first time an element takes a key, which is the first
    /// moment the compact shape knows the element will not collapse to its
    /// text.
    fn ensure_object(&mut self, depth: usize) {
        self.step(|scope, stack| {
            let element = scope.object()?;
            scope.set_index(stack, depth, element)
        });
    }

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

        // The compact shape does not know yet whether this element becomes an
        // object or collapses to its text, and a leaf with neither attributes
        // nor child elements collapses. Allocating here would throw that
        // object away for every such leaf, which in a document of records is
        // most of them; `ensure_object` allocates at the first key instead.
        if self.shape == Shape::Node {
            self.vm.step(|scope, stack| {
                let element = scope.object()?;
                let element_name = scope.string(&name)?;
                scope.set(element, "name", element_name)?;
                let attributes = scope.object()?;
                scope.set(element, "attributes", attributes)?;
                let children = scope.array(0)?;
                scope.set(element, "children", children)?;
                scope.set_index(stack, depth, element)
            });
        }
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
        if shape == Shape::Compact && self.frames[depth].keys == 1 {
            self.vm.ensure_object(depth);
        }
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
            if shape == Shape::Compact && self.frames[depth - 1].keys == 1 {
                self.vm.ensure_object(depth - 1);
            }
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

/// Write a value as an XML document, in whichever shape it is written in.
///
/// `replacer` is a function applied to each key and value, or a list of the
/// keys to keep, as `JSON.stringify` takes it. `space` is a string, or a count
/// of spaces, that one level of nesting indents by; content that is not
/// entirely made of elements is never indented, since white space there is
/// part of the document.
///
/// # Errors
/// A value that is not a document, or a name XML cannot spell, raises a
/// `TypeError`.
pub fn stringify(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let input = scope.argument(args, 0);
        let replacer = scope.argument(args, 1);
        let space = scope.argument(args, 2);
        let indent = read_indent(&mut scope, space)?;
        let filter = Filter::read(&mut scope, replacer)?;
        let document = if is_node_shape(&mut scope, input)? {
            let node = read_node(&mut scope, input, &filter, 0)?;
            otter_xml::stringify::node(&node, indent.as_deref())
        } else {
            let Some(value) = read_value(&mut scope, input, &filter, 0)? else {
                return Err(NativeError::TypeError {
                    name: WRITE,
                    reason: "expected an object naming one element".to_owned(),
                });
            };
            otter_xml::stringify::value(&value, indent.as_deref())
        };
        let document = document.map_err(write_error)?;
        let text = scope.string(&document)?;
        Ok(scope.finish(text))
    })
}

fn write_error(error: otter_xml::Error) -> NativeError {
    NativeError::TypeError {
        name: WRITE,
        reason: error.kind.to_string(),
    }
}

fn depth_error() -> NativeError {
    NativeError::TypeError {
        name: WRITE,
        reason: "the value nests too deeply, or refers to itself".to_owned(),
    }
}

/// The text one level of nesting indents by.
fn read_indent(
    scope: &mut NativeScope<'_, '_>,
    space: Local<'_>,
) -> Result<Option<String>, NativeError> {
    if scope.is_string(space) {
        let text: String = scope.string_value(space)?.chars().take(10).collect();
        return Ok((!text.is_empty()).then_some(text));
    }
    let Ok(count) = scope.number_value(space) else {
        return Ok(None);
    };
    if !count.is_finite() || count < 1.0 {
        return Ok(None);
    }
    Ok(Some(" ".repeat(count.min(10.0) as usize)))
}

/// What the caller asked to be left out, or rewritten, on the way.
enum Filter<'s> {
    /// Everything is written as it stands.
    All,
    /// A function of key and value, applied as `JSON.stringify` applies it.
    Function(Local<'s>),
    /// The keys to keep.
    Keys(Vec<String>),
}

impl<'s> Filter<'s> {
    fn read(
        scope: &mut NativeScope<'s, '_>,
        replacer: Local<'s>,
    ) -> Result<Filter<'s>, NativeError> {
        if scope.is_callable(replacer) {
            return Ok(Filter::Function(replacer));
        }
        if scope.is_array(replacer)? {
            let length = scope.array_length(replacer)?;
            let mut keys = Vec::with_capacity(length);
            for index in 0..length {
                let key = scope.index(replacer, index)?;
                if scope.is_string(key) {
                    keys.push(scope.string_value(key)?);
                }
            }
            return Ok(Filter::Keys(keys));
        }
        Ok(Filter::All)
    }

    /// Whether a key of an object survives a list of keys to keep.
    fn keeps(&self, key: &str) -> bool {
        match self {
            Filter::Keys(keys) => keys.iter().any(|kept| kept == key),
            _ => true,
        }
    }

    /// The value to write for `key`, once a function replacer has seen it.
    fn apply<'v>(
        &self,
        scope: &mut NativeScope<'v, '_>,
        holder: Local<'_>,
        key: &str,
        value: Local<'v>,
    ) -> Result<Local<'v>, NativeError> {
        let Filter::Function(function) = self else {
            return Ok(value);
        };
        let key = scope.string(key)?;
        scope.call(*function, holder, &[key, value])
    }
}

/// Whether a value is written in the shape that keeps document order.
fn is_node_shape(scope: &mut NativeScope<'_, '_>, value: Local<'_>) -> Result<bool, NativeError> {
    if !scope.is_object(value) || scope.is_array(value)? {
        return Ok(false);
    }
    if !scope.has_own_string_property(value, "name") {
        return Ok(false);
    }
    let name = scope.get(value, "name")?;
    Ok(scope.is_string(name)
        && (scope.has_own_string_property(value, "children")
            || scope.has_own_string_property(value, "attributes")))
}

/// Read one element of the document-order shape.
fn read_node(
    scope: &mut NativeScope<'_, '_>,
    value: Local<'_>,
    filter: &Filter<'_>,
    depth: usize,
) -> Result<otter_xml::Node, NativeError> {
    if depth >= MAX_VALUE_DEPTH {
        return Err(depth_error());
    }
    scope.scope(|mut scope| {
        let name = scope.get(value, "name")?;
        let mut node = otter_xml::Node {
            name: scope.string_value(name)?,
            ..otter_xml::Node::default()
        };
        let attributes = scope.get(value, "attributes")?;
        if scope.is_object(attributes) {
            for key in scope.enumerable_own_string_keys(attributes)? {
                if !filter.keeps(&key) {
                    continue;
                }
                let attribute = scope.get(attributes, &key)?;
                let attribute = filter.apply(&mut scope, attributes, &key, attribute)?;
                let Some(text) = primitive_text(&mut scope, attribute)? else {
                    continue;
                };
                node.attributes.push((key, text));
            }
        }
        let children = scope.get(value, "children")?;
        if scope.is_array(children)? {
            for index in 0..scope.array_length(children)? {
                let child = scope.index(children, index)?;
                let child = filter.apply(&mut scope, children, &index.to_string(), child)?;
                if scope.is_object(child) {
                    let element = read_node(&mut scope, child, filter, depth + 1)?;
                    node.children.push(otter_xml::Child::Element(element));
                    continue;
                }
                if let Some(text) = primitive_text(&mut scope, child)? {
                    node.children.push(otter_xml::Child::Text(text));
                }
            }
        }
        Ok(node)
    })
}

/// Read one value of the compact shape, or nothing where `JSON.stringify`
/// would write nothing.
fn read_value(
    scope: &mut NativeScope<'_, '_>,
    value: Local<'_>,
    filter: &Filter<'_>,
    depth: usize,
) -> Result<Option<otter_xml::Value>, NativeError> {
    if depth >= MAX_VALUE_DEPTH {
        return Err(depth_error());
    }
    if scope.is_array(value)? {
        return scope.scope(|mut scope| {
            let mut items = Vec::with_capacity(scope.array_length(value)?);
            for index in 0..scope.array_length(value)? {
                let item = scope.index(value, index)?;
                let item = filter.apply(&mut scope, value, &index.to_string(), item)?;
                if let Some(item) = read_value(&mut scope, item, filter, depth + 1)? {
                    items.push(item);
                }
            }
            Ok(Some(otter_xml::Value::Array(items)))
        });
    }
    if scope.is_object(value) && !scope.is_callable(value) {
        return scope.scope(|mut scope| {
            let keys = scope.enumerable_own_string_keys(value)?;
            let mut entries = Vec::with_capacity(keys.len());
            for key in keys {
                if !filter.keeps(&key) {
                    continue;
                }
                let entry = scope.get(value, &key)?;
                let entry = filter.apply(&mut scope, value, &key, entry)?;
                if let Some(entry) = read_value(&mut scope, entry, filter, depth + 1)? {
                    entries.push((key, entry));
                }
            }
            Ok(Some(otter_xml::Value::Object(entries)))
        });
    }
    Ok(primitive_text(scope, value)?.map(otter_xml::Value::Text))
}

/// The text a primitive is written as, or nothing for what is left out:
/// `undefined`, `null` and functions, as `JSON.stringify` leaves them out.
fn primitive_text(
    scope: &mut NativeScope<'_, '_>,
    value: Local<'_>,
) -> Result<Option<String>, NativeError> {
    if scope.is_string(value) {
        return Ok(Some(scope.string_value(value)?));
    }
    if scope.is_undefined(value) || scope.is_null(value) || scope.is_callable(value) {
        return Ok(None);
    }
    if let Ok(boolean) = scope.boolean_value(value) {
        return Ok(Some(boolean.to_string()));
    }
    if let Ok(number) = scope.number_value(value) {
        if !number.is_finite() {
            return Ok(None);
        }
        let text = if number.fract() == 0.0 && number.abs() < 1e21 {
            format!("{number:.0}")
        } else {
            number.to_string()
        };
        return Ok(Some(text));
    }
    Err(NativeError::TypeError {
        name: WRITE,
        reason: "a document holds text, not this".to_owned(),
    })
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
    runtime.install_native_global_call(
        "__otterXmlStringify",
        3,
        RuntimeNativeCall::Static(stringify),
    )?;
    runtime.install_script(SourceInput::from_javascript(
        r#"
        (function (g) {
          'use strict';
          var parse = g.__otterXmlParse;
          var stringify = g.__otterXmlStringify;
          delete g.__otterXmlParse;
          delete g.__otterXmlStringify;
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
          Object.defineProperty(xml, 'stringify', {
            value: stringify,
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
