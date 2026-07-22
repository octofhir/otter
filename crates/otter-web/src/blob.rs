//! WHATWG Blob / File host classes.
//!
//! Declared through `#[js_class]`: the impl signatures are the JS
//! surface, argument extraction and return construction ride the
//! marshalling layer, and prototype linkage / `instanceof` /
//! `Symbol.toStringTag` / JS-subclass `new.target` handling come from
//! the declaration. `File` is a native subclass — its data embeds the
//! `Blob` record and the ancestry walk lets every `Blob.prototype`
//! member run on `File` instances unmodified.
//!
//! # Contents
//! - [`Blob`] — bytes + normalized MIME type; the Rust-side record is
//!   also the JS instance data.
//! - [`File`] — `File extends Blob` with `name` / `lastModified`.
//! - [`BlobPart`] / [`BlobPropertyBag`] / [`FilePropertyBag`] — WebIDL
//!   argument shapes for the constructors.
//!
//! # Invariants
//! - Blob bytes are immutable snapshots (`Arc<[u8]>`): every part is
//!   copied at construction time per the File API, and clones (async
//!   reads, `slice`) share the buffer instead of copying again.
//! - `type` normalization matches the spec: any byte outside
//!   0x20–0x7E empties the type, otherwise it lowercases.
//!
//! # See also
//! - <https://w3c.github.io/FileAPI/>
//! - `docs/site/src/content/docs/extensions/declarative-bindings.md`

use std::sync::Arc;

use otter_macros::{FromJs, HostClass, js_class};
use otter_runtime::marshal::{ArrayBuffer, BufferSource, Sequence, USVString, Uint8Array};

/// Owned Blob record: immutable bytes + normalized MIME type.
#[derive(Debug, Clone, PartialEq, Eq, HostClass)]
pub struct Blob {
    bytes: Arc<[u8]>,
    content_type: String,
}

/// One `BlobPart`: a nested Blob (its bytes), a `BufferSource` (the
/// live byte range, copied), or anything else coerced to a string and
/// UTF-8 encoded. Probe order is declaration order; the string
/// coercion is the catch-all and stays last.
#[derive(FromJs)]
pub enum BlobPart {
    /// A nested `Blob` / `File` contributes its raw bytes.
    Blob(Blob),
    /// An `ArrayBuffer` or typed-array view contributes its bytes.
    Buffer(BufferSource),
    /// Everything else stringifies.
    Text(USVString),
}

impl BlobPart {
    fn append_to(self, out: &mut Vec<u8>) {
        match self {
            Self::Blob(blob) => out.extend_from_slice(&blob.bytes),
            Self::Buffer(buffer) => out.extend_from_slice(buffer.as_ref()),
            Self::Text(text) => out.extend_from_slice(text.as_str().as_bytes()),
        }
    }
}

/// The Blob constructor options bag (`{ type }`; `endings` is
/// "transparent"-only and therefore ignored).
#[derive(Debug, Default, FromJs)]
pub struct BlobPropertyBag {
    #[js(name = "type", default)]
    content_type: USVString,
}

/// The File constructor options bag (`{ type, lastModified }`).
#[derive(Debug, Default, FromJs)]
pub struct FilePropertyBag {
    #[js(name = "type", default)]
    content_type: USVString,
    #[js(name = "lastModified")]
    last_modified: Option<f64>,
}

impl Blob {
    /// Create a Blob from owned bytes and a MIME type (Rust-side API).
    #[must_use]
    pub fn new(bytes: Vec<u8>, content_type: impl Into<String>) -> Self {
        Self {
            bytes: bytes.into(),
            content_type: normalize_type(&content_type.into()),
        }
    }

    /// Byte length.
    #[must_use]
    pub fn size(&self) -> usize {
        self.bytes.len()
    }

    /// Normalized MIME type.
    #[must_use]
    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    /// Borrow the bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Slice with resolved (non-negative, clamped) bounds (Rust-side
    /// API). See [`Blob::js_slice`] for the Web-facing variant with
    /// relative-index semantics.
    #[must_use]
    pub fn slice(&self, start: usize, end: Option<usize>, content_type: Option<&str>) -> Self {
        let end = end.unwrap_or(self.bytes.len()).min(self.bytes.len());
        let start = start.min(end);
        Self {
            bytes: self.bytes[start..end].into(),
            content_type: normalize_type(content_type.unwrap_or(&self.content_type)),
        }
    }

    /// Bytes as UTF-8 with replacement.
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }

    fn from_parts(parts: Option<Sequence<BlobPart>>, options: Option<BlobPropertyBag>) -> Self {
        let mut bytes = Vec::new();
        for part in parts.into_iter().flatten() {
            part.append_to(&mut bytes);
        }
        let content_type = options.unwrap_or_default().content_type;
        Self::new(bytes, content_type.into_string())
    }
}

#[js_class(name = "Blob", feature = WEB)]
impl Blob {
    #[constructor]
    fn js_new(parts: Option<Sequence<BlobPart>>, options: Option<BlobPropertyBag>) -> Blob {
        Blob::from_parts(parts, options)
    }

    #[getter(name = "size")]
    fn js_size(&self) -> f64 {
        self.bytes.len() as f64
    }

    #[getter(name = "type")]
    fn js_type(&self) -> String {
        self.content_type.clone()
    }

    #[method(name = "slice", length = 2)]
    fn js_slice(
        &self,
        start: Option<f64>,
        end: Option<f64>,
        content_type: Option<USVString>,
    ) -> Blob {
        let size = self.bytes.len();
        let start = resolve_relative_index(start, size, 0);
        let end = resolve_relative_index(end, size, size);
        let span = end.saturating_sub(start);
        self.slice(
            start,
            Some(start + span),
            content_type.as_ref().map(|t| t.as_str()),
        )
    }

    #[method(name = "arrayBuffer")]
    async fn js_array_buffer(self) -> ArrayBuffer {
        ArrayBuffer(self.bytes.to_vec())
    }

    #[method(name = "bytes")]
    async fn js_bytes(self) -> Uint8Array {
        Uint8Array(self.bytes.to_vec())
    }

    #[method(name = "text")]
    async fn js_text(self) -> String {
        self.text()
    }
}

/// Owned File record: the embedded [`Blob`] plus file metadata.
#[derive(Debug, Clone, PartialEq, HostClass)]
pub struct File {
    #[host_class(parent)]
    blob: Blob,
    name: String,
    last_modified: f64,
}

impl File {
    /// File name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Milliseconds since the Unix epoch.
    #[must_use]
    pub fn last_modified(&self) -> f64 {
        self.last_modified
    }

    /// The embedded Blob record.
    #[must_use]
    pub fn as_blob(&self) -> &Blob {
        &self.blob
    }
}

#[js_class(name = "File", feature = WEB, extends = Blob)]
impl File {
    #[constructor]
    fn js_new(bits: Sequence<BlobPart>, name: USVString, options: Option<FilePropertyBag>) -> File {
        let options = options.unwrap_or_default();
        let blob = Blob::from_parts(
            Some(bits),
            Some(BlobPropertyBag {
                content_type: options.content_type,
            }),
        );
        File {
            blob,
            name: name.into_string(),
            last_modified: options.last_modified.unwrap_or_else(unix_millis_now),
        }
    }

    #[getter(name = "name")]
    fn js_name(&self) -> String {
        self.name.clone()
    }

    #[getter(name = "lastModified")]
    fn js_last_modified(&self) -> f64 {
        self.last_modified
    }
}

/// §3 File API: relative index resolution for `Blob.prototype.slice`
/// (negative counts from the end; NaN reads as 0; everything clamps
/// to `[0, size]`).
fn resolve_relative_index(index: Option<f64>, size: usize, default: usize) -> usize {
    let Some(index) = index else { return default };
    if index.is_nan() {
        return 0;
    }
    let size_f = size as f64;
    let resolved = if index < 0.0 {
        (size_f + index.trunc()).max(0.0)
    } else {
        index.trunc().min(size_f)
    };
    resolved as usize
}

/// Milliseconds since the Unix epoch, the `lastModified` default.
fn unix_millis_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// §3.1 File API `type` normalization: any byte outside 0x20–0x7E
/// empties the type; otherwise it lowercases.
fn normalize_type(value: &str) -> String {
    if value.bytes().all(|byte| (0x20..=0x7e).contains(&byte)) {
        value.to_ascii_lowercase()
    } else {
        String::new()
    }
}
