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
//! - The handle arena stays bounded. Node-form parsing keeps one rooted frame
//!   array; compact-form parsing keeps not-yet-published properties in a
//!   recyclable pending-root arena. Every event uses a nested handle scope, and
//!   a compact object takes ownership of its complete value prefix at close.
//! - No intermediate tree is built: the scanner's events drive the JavaScript
//!   values directly, so a document is walked once.
//! - Text runs are accumulated in Rust, not as JavaScript strings, so an
//!   element split across several runs still yields one string.
//! - Attribute values are interned for one parse and kept in the same pending
//!   root arena. Repeated spellings therefore allocate one JavaScript string,
//!   while unique values add no long-lived state beyond the result itself.
//! - Untouched runs from an ASCII JavaScript input become width-preserving
//!   substring views over that input. Rewritten runs and byte-backed inputs
//!   allocate standalone strings, so scanner offsets are never misapplied.
//! - Writing goes the other way through one owning Rust tree, so every rule
//!   about escaping, legal names and layout lives in `otter_xml::stringify`
//!   and not in two places. Reading a JavaScript value is bounded by an
//!   explicit depth, so a cyclic value fails rather than recurses forever.
//!
//! # See also
//! - [Handle scopes](../../../docs/site/src/content/docs/extensions/handle-scopes.md)
//! - `otter_xml::sink` — the events consumed here.

use std::borrow::Cow;

use rustc_hash::FxHashMap;

use otter_runtime::{
    OtterError, RuntimeExtensionContext, RuntimeExtensionInstaller, RuntimeHostAtom as HostAtom,
    RuntimeLocal as Local, RuntimeNativeCall, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeNativeScope as NativeScope,
    RuntimePendingValue as PendingValue, RuntimePendingValues as PendingValues,
    RuntimeValue as Value, SourceInput,
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
            if let Some(bytes) = scope.ascii_string_bytes(input)? {
                // ASCII is simultaneously UTF-8, Latin-1, and one UTF-16 code
                // unit per byte. Scanner source offsets can therefore address
                // O(1) slices of the original JavaScript string exactly.
                build::<Utf8>(&mut scope, &bytes, shape, Some(input))?
            } else {
                let text = scope.string_value(input)?;
                build::<Utf8>(&mut scope, text.as_bytes(), shape, None)?
            }
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
                Charset::Utf8 => build::<Utf8>(&mut scope, body, shape, None)?,
                Charset::Latin1 => build::<Latin1>(&mut scope, body, shape, None)?,
                Charset::Utf16Be | Charset::Utf16Le => {
                    let units = otter_xml::encoding::decode_utf16(
                        body,
                        sniffed.charset == Charset::Utf16Be,
                    )
                    .map_err(syntax_error)?;
                    build::<Utf16>(&mut scope, &units, shape, None)?
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
    source: Option<Local<'s>>,
) -> Result<Local<'s>, NativeError> {
    // Node-form parsing retains the existing rooted frame stack. Compact-form
    // values wait in the recyclable pending arena below and need no JS array.
    let stack = match shape {
        Shape::Node => scope.array(0)?,
        Shape::Compact => scope.value(Value::undefined()),
    };
    scope.with_pending_values(|scope, pending| {
        let (outcome, failure, root_atom, compact_root) = {
            let mut builder = Builder::<E> {
                vm: Vm {
                    scope,
                    stack,
                    pending,
                    failure: None,
                },
                shape,
                source,
                depth: 0,
                frames: Vec::new(),
                key: String::new(),
                attribute_values: FxHashMap::default(),
                root_atom: None,
                compact_root: None,
                encoding: std::marker::PhantomData,
            };
            let outcome = otter_xml::scan::parse::<E, _>(document, &mut builder);
            (
                outcome,
                builder.vm.failure.take(),
                builder.root_atom,
                builder.compact_root,
            )
        };
        outcome.map_err(syntax_error)?;
        if let Some(failure) = failure {
            return Err(failure);
        }
        match shape {
            Shape::Node => scope.index(stack, 0),
            Shape::Compact => {
                let root_atom = root_atom.expect("root element opened");
                let root = scope
                    .local_pending_value(pending, compact_root.expect("root element closed"))
                    .expect("live compact root");
                let layout = scope.object_layout_for_atoms(&root_atom, &[&root_atom])?;
                let wrapper = scope.object_with_layout(layout, &[root])?;
                let _ = scope
                    .release_pending_value(pending, compact_root.expect("root element closed"));
                Ok(wrapper)
            }
        }
    })
}

/// Mutator state shared by scanner event callbacks.
///
/// Kept apart from the rest of the builder so that a step can borrow the
/// scope mutably while still reading the builder's Rust-side buffers.
struct Vm<'a, 's, 'rt> {
    scope: &'a mut NativeScope<'s, 'rt>,
    stack: Local<'s>,
    pending: &'a mut PendingValues,
    failure: Option<NativeError>,
}

impl Vm<'_, '_, '_> {
    /// Intern one parser name in the owning isolate's host atom table.
    fn atom(&self, name: &str) -> HostAtom {
        self.scope.atom(name)
    }

    /// Run `body` in a nested handle scope, keeping the first failure.
    ///
    /// The nested scope bounds transient handles. Node values remain reachable
    /// from `stack`; compact values that are not in an object yet remain in
    /// `pending`.
    fn step<R>(
        &mut self,
        body: impl FnOnce(
            &mut NativeScope<'_, '_>,
            Local<'_>,
            &mut PendingValues,
        ) -> Result<R, NativeError>,
    ) -> Option<R> {
        if self.failure.is_some() {
            return None;
        }
        let stack = self.stack;
        let pending = &mut *self.pending;
        match self
            .scope
            .scope(|mut child| body(&mut child, stack, pending))
        {
            Ok(result) => Some(result),
            Err(error) => {
                self.failure = Some(error);
                None
            }
        }
    }
}

#[derive(Debug)]
struct PendingProperty {
    key: HostAtom,
    value: PendingValue,
    repeated_len: Option<usize>,
    /// The parse-wide attribute-value cache owns this pending root.
    retained: bool,
}

/// What is known about one open element while its children arrive.
#[derive(Default)]
struct Frame {
    /// The element's name, needed by its parent when it closes.
    name: String,
    /// Stable atom for `name`, used by compact child signatures.
    atom: Option<HostAtom>,
    /// Character data seen so far, joined across runs.
    text: String,
    /// Exact source range when `text` is one untouched scanner run.
    text_source: Option<(usize, usize)>,
    /// Compact-form properties accumulated before one-shot materialization.
    properties: Vec<PendingProperty>,
    /// How many children the node form has appended.
    children: usize,
}

struct Builder<'a, 's, 'rt, E: Encoding> {
    vm: Vm<'a, 's, 'rt>,
    shape: Shape,
    /// Original ASCII JavaScript input whose UTF-16 offsets equal scanner bytes.
    source: Option<Local<'s>>,
    depth: usize,
    frames: Vec<Frame>,
    /// Reused buffer for the `@name` key of an attribute.
    key: String,
    /// One collector-rooted JavaScript string per distinct attribute value.
    attribute_values: FxHashMap<Box<str>, PendingValue>,
    /// The root element's atom, which the compact form keys its result by.
    root_atom: Option<HostAtom>,
    /// Compact root retained until the one-property document wrapper is built.
    compact_root: Option<PendingValue>,
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

/// Exact document range of an untouched scanner run.
fn source_range<U: otter_xml::encoding::Unit>(piece: Piece<'_, U>) -> Option<(usize, usize)> {
    match piece {
        Piece::Source { units, offset } => Some((offset, units.len())),
        Piece::Rewritten(_) | Piece::Widened(_) => None,
    }
}

impl<E: Encoding> Builder<'_, '_, '_, E> {
    /// Return the parse-wide JavaScript string for one attribute value.
    fn attribute_value(
        &mut self,
        value: &str,
        source_range: Option<(usize, usize)>,
    ) -> Option<PendingValue> {
        if let Some(cached) = self.attribute_values.get(value) {
            return Some(*cached);
        }
        let source = self.source;
        let pending = self.vm.step(|scope, _stack, pending| {
            let text = match (source, source_range) {
                (Some(source), Some((start, len))) => {
                    scope.slice_string(source, start as u32, len as u32)?
                }
                _ => scope.string(value)?,
            };
            Ok(scope.pending_value(pending, text))
        })?;
        self.attribute_values.insert(value.into(), pending);
        Some(pending)
    }

    fn append_compact_child(&mut self, parent_depth: usize, key: HostAtom, child: PendingValue) {
        let existing = self.frames[parent_depth]
            .properties
            .iter()
            .position(|property| property.key == key);
        let Some(index) = existing else {
            self.frames[parent_depth].properties.push(PendingProperty {
                key,
                value: child,
                repeated_len: None,
                retained: false,
            });
            return;
        };

        let property = &self.frames[parent_depth].properties[index];
        let current = property.value;
        let repeated_len = property.repeated_len;
        let replacement = self.vm.step(|scope, _stack, pending| {
            let child_local = scope
                .local_pending_value(pending, child)
                .expect("live child pending root");
            if let Some(at) = repeated_len {
                let array = scope
                    .local_pending_value(pending, current)
                    .expect("live repeated-child array");
                scope.set_index(array, at, child_local)?;
                let _ = scope.release_pending_value(pending, child);
                return Ok(None);
            }

            let previous = scope
                .local_pending_value(pending, current)
                .expect("live first child");
            let array = scope.array(0)?;
            scope.set_index(array, 0, previous)?;
            scope.set_index(array, 1, child_local)?;
            let replacement = scope.pending_value(pending, array);
            let _ = scope.release_pending_value(pending, current);
            let _ = scope.release_pending_value(pending, child);
            Ok(Some(replacement))
        });
        if self.vm.failure.is_some() {
            return;
        }
        let property = &mut self.frames[parent_depth].properties[index];
        match replacement.flatten() {
            Some(replacement) => {
                property.value = replacement;
                property.repeated_len = Some(2);
            }
            None => property.repeated_len = repeated_len.map(|len| len + 1),
        }
    }

    fn end_compact_element(&mut self, depth: usize) {
        let frame = &mut self.frames[depth];
        let element_atom = frame.atom.clone().expect("open element atom");
        let content = trimmed(&frame.text);
        let leading = content.as_ptr() as usize - frame.text.as_ptr() as usize;
        let text_source = frame
            .text_source
            .map(|(start, _)| (start + leading, content.len()));
        let source = self.source;
        let mut properties = std::mem::take(&mut frame.properties);
        let value = self.vm.step(|scope, _stack, pending| {
            let value = if properties.is_empty() {
                match (source, text_source) {
                    (Some(source), Some((start, len))) => {
                        scope.slice_string(source, start as u32, len as u32)?
                    }
                    _ => scope.string(content)?,
                }
            } else {
                if !content.is_empty() {
                    let text = match (source, text_source) {
                        (Some(source), Some((start, len))) => {
                            scope.slice_string(source, start as u32, len as u32)?
                        }
                        _ => scope.string(content)?,
                    };
                    let text = scope.pending_value(pending, text);
                    properties.push(PendingProperty {
                        key: scope.atom("#text"),
                        value: text,
                        repeated_len: None,
                        retained: false,
                    });
                }
                let keys: Vec<&HostAtom> =
                    properties.iter().map(|property| &property.key).collect();
                let values: Vec<Local<'_>> = properties
                    .iter()
                    .map(|property| {
                        scope
                            .local_pending_value(pending, property.value)
                            .expect("live compact property")
                    })
                    .collect();
                let layout = scope.object_layout_for_atoms(&element_atom, &keys)?;
                let object = scope.object_with_layout(layout, &values)?;
                for property in &properties {
                    if !property.retained {
                        let _ = scope.release_pending_value(pending, property.value);
                    }
                }
                object
            };
            Ok(scope.pending_value(pending, value))
        });
        let Some(value) = value else {
            return;
        };
        if depth == 0 {
            self.compact_root = Some(value);
        } else {
            self.append_compact_child(depth - 1, element_atom, value);
        }
    }
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
        frame.text_source = None;
        debug_assert!(frame.properties.is_empty());
        frame.children = 0;
        frame.name.clear();
        frame.name.push_str(&name);
        let atom = self.vm.atom(&name);
        frame.atom = Some(atom.clone());
        if depth == 0 {
            self.root_atom = Some(atom);
        }

        if self.shape == Shape::Node {
            self.vm.step(|scope, stack, _pending| {
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
        let value_source = self.source.and_then(|_| source_range(value));
        let value = text_of::<E>(value);
        let depth = self.depth - 1;
        let shape = self.shape;
        let Some(text) = self.attribute_value(&value, value_source) else {
            return;
        };
        if shape == Shape::Compact {
            self.key.clear();
            self.key.push('@');
            self.key.push_str(&name);
            let key = self.vm.atom(&self.key);
            self.frames[depth].properties.push(PendingProperty {
                key,
                value: text,
                repeated_len: None,
                retained: true,
            });
            return;
        }
        self.vm.step(|scope, stack, pending| {
            let element = scope.index(stack, depth)?;
            let text = scope
                .local_pending_value(pending, text)
                .expect("interned attribute value");
            let attributes = scope.get(element, "attributes")?;
            scope.set(attributes, &name, text)
        });
    }

    fn text(&mut self, text: Piece<'_, E::Unit>) {
        let depth = self.depth - 1;
        let text_source = self.source.and_then(|_| source_range(text));
        let run = text_of::<E>(text);
        if self.shape == Shape::Compact {
            let frame = &mut self.frames[depth];
            if !run.is_empty() {
                if frame.text.is_empty() {
                    frame.text_source = text_source;
                } else {
                    frame.text_source = None;
                }
                frame.text.push_str(&run);
            }
            return;
        }
        let at = self.frames[depth].children;
        self.frames[depth].children += 1;
        let source = self.source;
        self.vm.step(|scope, stack, _pending| {
            let element = scope.index(stack, depth)?;
            let children = scope.get(element, "children")?;
            let run = match (source, text_source) {
                (Some(source), Some((start, len))) => {
                    scope.slice_string(source, start as u32, len as u32)?
                }
                _ => scope.string(&run)?,
            };
            scope.set_index(children, at, run)
        });
    }

    fn end_element(&mut self) {
        self.depth -= 1;
        let depth = self.depth;
        if self.shape == Shape::Compact {
            self.end_compact_element(depth);
            return;
        }
        let parent_children = if depth > 0 {
            let at = self.frames[depth - 1].children;
            self.frames[depth - 1].children += 1;
            at
        } else {
            0
        };
        self.vm.step(|scope, stack, _pending| {
            let element = scope.index(stack, depth)?;
            if depth == 0 {
                return scope.set_index(stack, 0, element);
            }
            let parent = scope.index(stack, depth - 1)?;
            let children = scope.get(parent, "children")?;
            scope.set_index(children, parent_children, element)
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
