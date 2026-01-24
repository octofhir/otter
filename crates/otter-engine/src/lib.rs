//! Otter Engine - Embeddable TypeScript/JavaScript runtime
//!
//! Main entry point for using Otter in Rust applications.
//!
//! # Features
//!
//! - **ESM Module Loading**: Load ES modules from file://, node:, and https:// URLs
//! - **Security**: Capability-based permissions and allowlist for remote modules
//! - **Caching**: In-memory and disk caching for loaded modules
//! - **Import Maps**: Support for module aliasing
//! - **Dependency Graph**: Cycle detection and topological sorting
//!
//! # Example
//!
//! ```ignore
//! use otter_engine::prelude::*;
//!
//! #[tokio::main]
//! async fn main() {
//!     // Recommended: Use EngineBuilder for automatic builtins
//!     let mut engine = EngineBuilder::new()
//!         .capabilities(CapabilitiesBuilder::new()
//!             .allow_net_all()
//!             .build())
//!         .env(|b| b.explicit("NODE_ENV", "production"))
//!         .with_http()  // Enable Otter.serve()
//!         .build();
//!
//!     engine.eval(r#"
//!         console.log("Hello from Otter!");
//!     "#).await.unwrap();
//! }
//! ```

// Own modules (ESM loader, graph)
pub mod error;
pub mod graph;
pub mod loader;

// Re-export own types
pub use error::{EngineError, EngineResult};
pub use graph::{ImportRecord, ModuleGraph, ModuleNode, parse_imports};
pub use loader::{
    ImportContext, LoaderConfig, ModuleLoader, ModuleType, ResolvedModule, SourceType,
};

// ============================================================================
// Re-exports from VM crates
// ============================================================================

// Re-export runtime types (main entry points)
pub use otter_vm_runtime::{
    // Event loop
    ActiveServerCount,
    // Capabilities and security
    Capabilities,
    CapabilitiesBuilder,
    CapabilitiesGuard,
    // Environment store
    DEFAULT_DENY_PATTERNS,
    EnvFileError,
    EnvStoreBuilder,
    EnvWriteError,
    EventLoop,
    // Extension system
    Extension,
    ExtensionRegistry,
    HttpEvent,
    WsEvent,
    IsolatedEnvStore,
    // Module loader
    LoadedModule,
    ModuleError,
    ModuleNamespace,
    ModuleState,
    NativeOpResult,
    Op,
    OpHandler,
    // Main runtime
    Otter,
    OtterBuilder,
    OtterError,
    PermissionDenied,
    // Promise and timers
    Promise,
    Timer,
    TimerId,
    // Workers
    Worker,
    WorkerContext,
    WorkerError,
    WorkerMessage,
    WorkerPool,
    module_extension,
    op_async,
    op_native,
    op_sync,
    parse_env_file,
};

// Re-export VM core types (for extension authors)
pub use otter_vm_core::{
    // Generator
    GeneratorContext,
    GeneratorState,
    // Context and runtime
    Interpreter,
    IteratorResult,
    JsGenerator,
    // Objects
    JsObject,
    // Promise
    JsPromise,
    // Proxy
    JsProxy,
    // Strings
    JsString,
    // Values
    NativeFn,
    PromiseState,
    PropertyKey,
    RevocableProxy,
    // Shared buffer
    SharedArrayBuffer,
    // Structured clone
    StructuredCloneError,
    StructuredCloner,
    Symbol,
    Value,
    VmContext,
    // Errors
    VmError,
    VmResult,
    VmRuntime,
    structured_clone,
};

// Re-export compiler (for advanced usage)
pub use otter_vm_compiler::{CompileError, CompileResult, Compiler};

// Re-export bytecode module
pub use otter_vm_bytecode::Module;

// Re-export builtin factories
pub use otter_vm_builtins::{
    ConsoleAdapter, LogLevel, StdConsole, create_builtins_extension,
    create_builtins_extension_with_console, create_http_extension,
};

// ============================================================================
// High-level Engine Builder (includes builtins automatically)
// ============================================================================

/// High-level engine builder that automatically registers standard builtins.
///
/// This is the recommended way to create an Otter runtime for most use cases.
/// It wraps `OtterBuilder` and automatically registers:
/// - Console (console.log, console.error, etc.)
/// - Math, JSON, Date, RegExp, and other standard objects
/// - fetch() for HTTP requests
/// - Optionally: HTTP server (Otter.serve())
///
/// # Example
///
/// ```ignore
/// use otter_engine::EngineBuilder;
///
/// // Basic runtime with all builtins
/// let mut engine = EngineBuilder::new().build();
/// engine.eval("console.log('Hello!')").await?;
///
/// // With HTTP server support
/// let mut engine = EngineBuilder::new()
///     .with_http()
///     .build();
///
/// // With permissions
/// use otter_engine::{CapabilitiesBuilder, EnvStoreBuilder};
///
/// let mut engine = EngineBuilder::new()
///     .capabilities(CapabilitiesBuilder::new()
///         .allow_net_all()
///         .build())
///     .env(|b| b.explicit("NODE_ENV", "production"))
///     .with_http()
///     .build();
/// ```
pub struct EngineBuilder {
    inner: OtterBuilder,
    with_http: bool,
}

impl EngineBuilder {
    /// Create a new engine builder with secure defaults.
    pub fn new() -> Self {
        Self {
            inner: OtterBuilder::new(),
            with_http: false,
        }
    }

    /// Enable HTTP server support (Otter.serve()).
    ///
    /// Note: fetch() is always available. This only enables the server API.
    pub fn with_http(mut self) -> Self {
        self.with_http = true;
        self
    }

    /// Set capabilities (permissions).
    ///
    /// By default, all capabilities are denied.
    pub fn capabilities(mut self, caps: Capabilities) -> Self {
        self.inner = self.inner.capabilities(caps);
        self
    }

    /// Set environment store directly.
    pub fn env_store(mut self, store: IsolatedEnvStore) -> Self {
        self.inner = self.inner.env_store(store);
        self
    }

    /// Configure environment store with a builder function.
    ///
    /// # Example
    ///
    /// ```ignore
    /// let engine = EngineBuilder::new()
    ///     .env(|b| b
    ///         .explicit("NODE_ENV", "production")
    ///         .passthrough(&["HOME", "USER"])
    ///     )
    ///     .build();
    /// ```
    pub fn env(mut self, f: impl FnOnce(EnvStoreBuilder) -> EnvStoreBuilder) -> Self {
        self.inner = self.inner.env(f);
        self
    }

    /// Add a custom extension.
    pub fn extension(mut self, ext: Extension) -> Self {
        self.inner = self.inner.extension(ext);
        self
    }

    /// Build the engine with all builtins registered.
    pub fn build(self) -> Otter {
        // Build base runtime (without builtins)
        let mut runtime = self.inner.build();

        // Register standard builtins (console, Math, JSON, Date, fetch, etc.)
        runtime
            .register_extension(create_builtins_extension())
            .expect("Failed to register builtins extension");

        // Register HTTP server if enabled
        if self.with_http {
            let http_ext = create_http_extension(
                runtime.http_event_sender(),
                runtime.ws_event_sender(),
                runtime.active_server_count(),
            );
            runtime
                .register_extension(http_ext)
                .expect("Failed to register HTTP extension");
        }

        runtime
    }
}

impl Default for EngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Prelude for common imports
pub mod prelude {
    pub use crate::{
        // Security
        Capabilities,
        CapabilitiesBuilder,
        // Main entry point (recommended)
        EngineBuilder,
        // Environment
        EnvStoreBuilder,
        // Extensions
        Extension,
        IsolatedEnvStore,
        // Values
        JsObject,
        JsString,
        // Module loading
        ModuleGraph,
        ModuleLoader,
        Op,
        // Low-level runtime (for advanced use)
        Otter,
        OtterBuilder,
        Value,
        op_async,
        op_sync,
    };
}
