//! Source admitted for a single synchronous compile-and-execute scope.
//!
//! # Contents
//! - `CompileSource` lends the exact text and, when it has a scope, its owner.
//! - `SourceCompileScope` releases optional source-specific compiler storage.
//!
//! # Invariants
//! The scoped and borrowed forms carry exactly one text. A compiler may
//! retain the same owner for the lifetime of a scope. Its cleanup runs on
//! every Rust return or unwind, including failed linking and execution, on
//! the thread that owns the retained compiler storage.
//!
//! # See also
//! - `super::SharedSource` owns the text and its one resource charge.

use std::marker::PhantomData;

use crate::SharedSource;

type Release = fn(&SharedSource);

/// Source text being compiled. Only a live execution scope can ask the
/// compiler to keep source-specific work available for first calls.
#[derive(Debug)]
pub enum CompileSource<'a> {
    /// A temporary or standalone compilation whose text is borrowed.
    Borrowed(&'a str),
    /// Exact source that an executing scope supplies when linking. The
    /// compiler can return source-text ranges without copying the text.
    Scoped {
        /// Owner that also supplies the eventual linked source text.
        source: &'a SharedSource,
        /// Cleanup for the retained compiler data, if the compiler keeps any.
        release: &'a mut Option<Release>,
        #[doc(hidden)]
        thread: &'a PhantomData<*const ()>,
    },
}

impl<'a> CompileSource<'a> {
    /// The same text the eventual execution will use.
    #[must_use]
    pub fn text(&self) -> &'a str {
        match self {
            Self::Borrowed(text) => text,
            Self::Scoped { source, .. } => AsRef::<str>::as_ref(*source),
        }
    }
}

/// Owns admitted source until its synchronous execution completes.
#[derive(Debug)]
pub struct SourceCompileScope {
    source: SharedSource,
    release: Option<Release>,
    // The cleanup callback can own thread-local parser storage.
    thread: PhantomData<*const ()>,
}

impl SourceCompileScope {
    /// Start a scope holding this exact source allocation.
    #[must_use]
    pub fn new(source: SharedSource) -> Self {
        Self {
            source,
            release: None,
            thread: PhantomData,
        }
    }

    /// Lend the input and its cleanup slot to the compiler.
    pub fn input(&mut self) -> CompileSource<'_> {
        CompileSource::Scoped {
            source: &self.source,
            release: &mut self.release,
            thread: &self.thread,
        }
    }

    /// Share the identical source with the linked code's source registry.
    #[must_use]
    pub fn source(&self) -> &SharedSource {
        &self.source
    }
}

impl Drop for SourceCompileScope {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release(&self.source);
        }
    }
}
