//! Executable module and function containers for the new VM.

use core::fmt;
use std::sync::Arc;

use crate::bigint::BigIntTable;
use crate::bytecode::Bytecode;
use crate::call::CallTable;
use crate::closure::ClosureTable;
use crate::deopt::DeoptTable;
use crate::exception::ExceptionTable;
use crate::feedback::FeedbackTableLayout;
use crate::float::FloatTable;
use crate::frame::FrameLayout;
use crate::property::PropertyNameTable;
use crate::regexp::RegExpTable;
use crate::source_map::SourceMap;
use crate::string::StringTable;

/// Stable function index inside a module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FunctionIndex(pub u32);

/// Errors produced while constructing an executable module.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleError {
    /// The module does not contain an entry function at the requested index.
    InvalidEntryFunction,
}

impl fmt::Display for ModuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEntryFunction => {
                f.write_str("module entry function index is out of bounds")
            }
        }
    }
}

impl std::error::Error for ModuleError {}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionSideTables {
    property_names: PropertyNameTable,
    string_literals: StringTable,
    float_constants: FloatTable,
    bigint_constants: BigIntTable,
    closures: ClosureTable,
    calls: CallTable,
    regexp_literals: RegExpTable,
}

impl FunctionSideTables {
    /// Creates the side-table group attached to a function.
    #[must_use]
    pub fn new(
        property_names: PropertyNameTable,
        string_literals: StringTable,
        float_constants: FloatTable,
        bigint_constants: BigIntTable,
        closures: ClosureTable,
        calls: CallTable,
        regexp_literals: RegExpTable,
    ) -> Self {
        Self {
            property_names,
            string_literals,
            float_constants,
            bigint_constants,
            closures,
            calls,
            regexp_literals,
        }
    }

    /// Returns the regexp-literal side table.
    #[must_use]
    pub fn regexp_literals(&self) -> &RegExpTable {
        &self.regexp_literals
    }
}

impl Default for FunctionSideTables {
    fn default() -> Self {
        Self::new(
            PropertyNameTable::default(),
            StringTable::default(),
            FloatTable::default(),
            BigIntTable::default(),
            ClosureTable::default(),
            CallTable::default(),
            RegExpTable::default(),
        )
    }
}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FunctionTables {
    side_tables: FunctionSideTables,
    feedback: FeedbackTableLayout,
    deopt: DeoptTable,
    exceptions: ExceptionTable,
    source_map: SourceMap,
}

impl FunctionTables {
    /// Creates an explicit side-table bundle for a function.
    #[must_use]
    pub fn new(
        side_tables: FunctionSideTables,
        feedback: FeedbackTableLayout,
        deopt: DeoptTable,
        exceptions: ExceptionTable,
        source_map: SourceMap,
    ) -> Self {
        Self {
            side_tables,
            feedback,
            deopt,
            exceptions,
            source_map,
        }
    }

    /// Creates an empty side-table bundle.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(
            FunctionSideTables::default(),
            FeedbackTableLayout::default(),
            DeoptTable::default(),
            ExceptionTable::default(),
            SourceMap::default(),
        )
    }
}

impl Default for FunctionTables {
    fn default() -> Self {
        Self::empty()
    }
}

/// Immutable executable function for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Function {
    name: Option<Box<str>>,
    length: u16,
    strict: bool,
    derived_constructor: bool,
    /// §27.3 — this function is a generator (`function*`).
    generator: bool,
    /// §27.7 — this function is async (`async function`).
    r#async: bool,
    frame_layout: FrameLayout,
    bytecode: Bytecode,
    property_names: PropertyNameTable,
    string_literals: StringTable,
    float_constants: FloatTable,
    bigint_constants: BigIntTable,
    closures: ClosureTable,
    calls: CallTable,
    regexp_literals: RegExpTable,
    feedback: FeedbackTableLayout,
    deopt: DeoptTable,
    exceptions: ExceptionTable,
    source_map: SourceMap,
}

impl Function {
    /// Creates an executable function with explicit side tables.
    #[must_use]
    pub fn new(
        name: Option<impl Into<Box<str>>>,
        frame_layout: FrameLayout,
        bytecode: Bytecode,
        tables: FunctionTables,
    ) -> Self {
        Self::new_with_length(
            name,
            frame_layout.parameter_count(),
            frame_layout,
            bytecode,
            tables,
        )
    }

    /// Creates an executable function with explicit `.length`.
    #[must_use]
    pub fn new_with_length(
        name: Option<impl Into<Box<str>>>,
        length: u16,
        frame_layout: FrameLayout,
        bytecode: Bytecode,
        tables: FunctionTables,
    ) -> Self {
        Self {
            name: name.map(Into::into),
            length,
            strict: false,
            derived_constructor: false,
            generator: false,
            r#async: false,
            frame_layout,
            bytecode,
            property_names: tables.side_tables.property_names,
            string_literals: tables.side_tables.string_literals,
            float_constants: tables.side_tables.float_constants,
            bigint_constants: tables.side_tables.bigint_constants,
            closures: tables.side_tables.closures,
            calls: tables.side_tables.calls,
            regexp_literals: tables.side_tables.regexp_literals,
            feedback: tables.feedback,
            deopt: tables.deopt,
            exceptions: tables.exceptions,
            source_map: tables.source_map,
        }
    }

    /// Creates a function with empty side tables.
    ///
    /// Shorthand for [`Self::new`] with [`FunctionTables::default`].
    #[must_use]
    pub fn with_empty_tables(
        name: Option<impl Into<Box<str>>>,
        frame_layout: FrameLayout,
        bytecode: Bytecode,
    ) -> Self {
        Self::new(name, frame_layout, bytecode, FunctionTables::default())
    }

    /// Replace the function's bytecode in place.
    ///
    /// Used by the compiler to swap in the final bytecode after the
    /// `Function` has been constructed with empty side tables.
    #[must_use]
    pub fn with_bytecode(mut self, bytecode: Bytecode) -> Self {
        self.bytecode = bytecode;
        self
    }

    /// Returns the function name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns the JS-visible function `.length`.
    #[must_use]
    pub const fn length(&self) -> u16 {
        self.length
    }

    /// Returns whether this function executes in strict mode.
    #[must_use]
    pub const fn is_strict(&self) -> bool {
        self.strict
    }

    /// Marks this function as strict or non-strict.
    #[must_use]
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Marks this function as a derived class constructor.
    #[must_use]
    pub fn with_derived_constructor(mut self, derived_constructor: bool) -> Self {
        self.derived_constructor = derived_constructor;
        self
    }

    /// Returns whether this function is a derived class constructor.
    #[must_use]
    pub const fn is_derived_constructor(&self) -> bool {
        self.derived_constructor
    }

    /// Builder-style setter for the generator flag.
    #[must_use]
    pub fn with_generator(mut self, generator: bool) -> Self {
        self.generator = generator;
        self
    }

    /// Returns whether this function is a generator (`function*`).
    /// Spec: <https://tc39.es/ecma262/#sec-generator-function-definitions>
    #[must_use]
    pub const fn is_generator(&self) -> bool {
        self.generator
    }

    /// Builder-style setter for the async flag.
    /// Spec: <https://tc39.es/ecma262/#sec-async-function-definitions>
    #[must_use]
    pub fn with_async(mut self, r#async: bool) -> Self {
        self.r#async = r#async;
        self
    }

    /// Returns whether this function is async (`async function`).
    /// Spec: <https://tc39.es/ecma262/#sec-async-function-definitions>
    #[must_use]
    pub const fn is_async(&self) -> bool {
        self.r#async
    }

    /// Returns the frame layout.
    #[must_use]
    pub const fn frame_layout(&self) -> FrameLayout {
        self.frame_layout
    }

    /// Returns the immutable bytecode stream attached to this function.
    ///
    /// The interpreter dispatches directly off this buffer; the JIT
    /// reads it once at compile-time to produce a stencil.
    #[must_use]
    pub fn bytecode(&self) -> &Bytecode {
        &self.bytecode
    }

    /// Returns the feedback side-table layout.
    #[must_use]
    pub fn feedback(&self) -> &FeedbackTableLayout {
        &self.feedback
    }

    /// Returns the property-name side table.
    #[must_use]
    pub fn property_names(&self) -> &PropertyNameTable {
        &self.property_names
    }

    /// Returns the string-literal side table.
    #[must_use]
    pub fn string_literals(&self) -> &StringTable {
        &self.string_literals
    }

    /// Returns the float-constant side table.
    #[must_use]
    pub fn float_constants(&self) -> &FloatTable {
        &self.float_constants
    }

    /// Returns the BigInt-constant side table.
    /// Spec: <https://tc39.es/ecma262/#sec-ecmascript-language-types-bigint-type>
    #[must_use]
    pub fn bigint_constants(&self) -> &BigIntTable {
        &self.bigint_constants
    }

    /// Returns the closure-creation side table.
    #[must_use]
    pub fn closures(&self) -> &ClosureTable {
        &self.closures
    }

    /// Returns the call-site side table.
    #[must_use]
    pub fn calls(&self) -> &CallTable {
        &self.calls
    }

    /// Returns the regexp-literal side table.
    #[must_use]
    pub fn regexp_literals(&self) -> &RegExpTable {
        &self.regexp_literals
    }

    /// Returns the deoptimization table.
    #[must_use]
    pub fn deopt(&self) -> &DeoptTable {
        &self.deopt
    }

    /// Returns the exception table.
    #[must_use]
    pub fn exceptions(&self) -> &ExceptionTable {
        &self.exceptions
    }

    /// Returns the source map.
    #[must_use]
    pub fn source_map(&self) -> &SourceMap {
        &self.source_map
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  §16.2.1.4 — Import/Export Records for Source Text Module Records
//  Spec: <https://tc39.es/ecma262/#sec-source-text-module-records>
// ═══════════════════════════════════════════════════════════════════════════

/// A single import binding within an `ImportRecord`.
/// Spec: <https://tc39.es/ecma262/#table-importentry-record-fields>
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportBinding {
    /// `import { imported as local } from "specifier"` — named import.
    Named {
        /// The exported name in the source module (e.g., `foo` in `import { foo }`).
        imported: Box<str>,
        /// The local binding name (e.g., `bar` in `import { foo as bar }`).
        local: Box<str>,
    },
    /// `import local from "specifier"` — default import.
    Default {
        /// The local binding name.
        local: Box<str>,
    },
    /// `import * as local from "specifier"` — namespace import.
    Namespace {
        /// The local binding name.
        local: Box<str>,
    },
}

impl ImportBinding {
    /// Returns the local binding name for this import.
    #[must_use]
    pub fn local_name(&self) -> &str {
        match self {
            Self::Named { local, .. } | Self::Default { local } | Self::Namespace { local } => {
                local
            }
        }
    }
}

/// A single import declaration record.
/// Spec: <https://tc39.es/ecma262/#sec-imports>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportRecord {
    /// The module specifier string (e.g., `"./utils.js"`, `"lodash"`).
    pub specifier: Box<str>,
    /// Individual import bindings from this specifier.
    pub bindings: Vec<ImportBinding>,
}

/// A single export record.
/// Spec: <https://tc39.es/ecma262/#sec-exports>
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportRecord {
    /// `export { local as exported }` or `export const exported = ...`.
    Named {
        /// The local binding name.
        local: Box<str>,
        /// The externally visible export name.
        exported: Box<str>,
    },
    /// `export default expr`.
    Default {
        /// The local binding name (often a synthetic `*default*`).
        local: Box<str>,
    },
    /// `export { imported as exported } from "specifier"` — re-export named.
    ReExportNamed {
        /// The source module specifier.
        specifier: Box<str>,
        /// The name imported from the source module.
        imported: Box<str>,
        /// The name exported from this module.
        exported: Box<str>,
    },
    /// `export * from "specifier"` — re-export all.
    ReExportAll {
        /// The source module specifier.
        specifier: Box<str>,
    },
    /// `export * as name from "specifier"` — namespace re-export.
    ReExportNamespace {
        /// The source module specifier.
        specifier: Box<str>,
        /// The exported namespace name.
        exported: Box<str>,
    },
}

/// Immutable executable module for the new VM.
///
/// §16.2 — Modules
/// Spec: <https://tc39.es/ecma262/#sec-modules>
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    name: Option<Box<str>>,
    functions: Box<[Function]>,
    entry: FunctionIndex,
    /// Whether this module uses ES module semantics (import/export).
    is_esm: bool,
    /// §16.2.2 — Static import records.
    /// Spec: <https://tc39.es/ecma262/#sec-imports>
    imports: Vec<ImportRecord>,
    /// §16.2.3 — Static export records.
    /// Spec: <https://tc39.es/ecma262/#sec-exports>
    exports: Vec<ExportRecord>,
    /// Original source text (TypeScript or JavaScript as written by the user).
    ///
    /// Populated by the source compiler with the *pre-transformation* source
    /// text so runtime diagnostics can render snippets that match what the
    /// developer typed byte-for-byte, not the post-codegen JavaScript. `None`
    /// for synthetic modules (host-installed shims, native modules, etc.)
    /// where no single source text is meaningful.
    source_text: Option<Arc<str>>,
}

impl Module {
    /// Creates a checked executable module.
    pub fn new(
        name: Option<impl Into<Box<str>>>,
        functions: Vec<Function>,
        entry: FunctionIndex,
    ) -> Result<Self, ModuleError> {
        let functions = functions.into_boxed_slice();
        if usize::try_from(entry.0)
            .ok()
            .and_then(|index| functions.get(index))
            .is_none()
        {
            return Err(ModuleError::InvalidEntryFunction);
        }

        Ok(Self {
            name: name.map(Into::into),
            functions,
            entry,
            is_esm: false,
            imports: Vec::new(),
            exports: Vec::new(),
            source_text: None,
        })
    }

    /// Creates a checked executable ES module with import/export records.
    pub fn new_esm(
        name: Option<impl Into<Box<str>>>,
        functions: Vec<Function>,
        entry: FunctionIndex,
        imports: Vec<ImportRecord>,
        exports: Vec<ExportRecord>,
    ) -> Result<Self, ModuleError> {
        let functions = functions.into_boxed_slice();
        if usize::try_from(entry.0)
            .ok()
            .and_then(|index| functions.get(index))
            .is_none()
        {
            return Err(ModuleError::InvalidEntryFunction);
        }

        Ok(Self {
            name: name.map(Into::into),
            functions,
            entry,
            is_esm: true,
            imports,
            exports,
            source_text: None,
        })
    }

    /// Attaches the original (pre-transform) source text to this module.
    ///
    /// Consumed by diagnostic rendering so error snippets show what the user
    /// actually wrote — identical for `.js` and pre-strip for `.ts`.
    #[must_use]
    pub fn with_source_text(mut self, source_text: Arc<str>) -> Self {
        self.source_text = Some(source_text);
        self
    }

    /// Returns the original source text, if one was attached at compile time.
    #[must_use]
    pub fn source_text(&self) -> Option<&Arc<str>> {
        self.source_text.as_ref()
    }

    /// Returns the module name, if present.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Returns whether this is an ES module (has import/export semantics).
    #[must_use]
    pub const fn is_esm(&self) -> bool {
        self.is_esm
    }

    /// Returns the static import records.
    /// Spec: <https://tc39.es/ecma262/#sec-imports>
    #[must_use]
    pub fn imports(&self) -> &[ImportRecord] {
        &self.imports
    }

    /// Returns the static export records.
    /// Spec: <https://tc39.es/ecma262/#sec-exports>
    #[must_use]
    pub fn exports(&self) -> &[ExportRecord] {
        &self.exports
    }

    /// Returns the number of functions in the module.
    #[must_use]
    pub fn function_count(&self) -> usize {
        self.functions.len()
    }

    /// Returns the entry function index.
    #[must_use]
    pub const fn entry(&self) -> FunctionIndex {
        self.entry
    }

    /// Returns the entry function.
    #[must_use]
    pub fn entry_function(&self) -> &Function {
        let index =
            usize::try_from(self.entry.0).expect("entry function index must fit into usize");
        &self.functions[index]
    }

    /// Returns the immutable function slice.
    #[must_use]
    pub fn functions(&self) -> &[Function] {
        &self.functions
    }

    /// Returns the function at the given index.
    #[must_use]
    pub fn function(&self, index: FunctionIndex) -> Option<&Function> {
        usize::try_from(index.0)
            .ok()
            .and_then(|position| self.functions.get(position))
    }
}
