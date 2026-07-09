//! Template for an embedder-defined Otter extension.
//!
//! Copy this crate, rename the surfaces, and register the results on
//! your runtime builder. It demonstrates every declarative form once:
//!
//! - a host class ([`Counter`]) with a constructor, getter, mutating
//!   method, async method, static method, and attached JS glue;
//! - a top-level namespace ([`Acme`], the `Acme` global);
//! - a hosted module (`acme:util`, importable from JS);
//! - the [`romp!`](otter_macros::romp) bundle tying the globals
//!   together.
//!
//! Registration is two builder calls:
//!
//! ```rust,ignore
//! let runtime = Runtime::builder()
//!     .extension(&ACME_EXTENSION)          // Counter + Acme + lazy JS
//!     .hosted_module(UTIL_HOSTED_MODULE)   // acme:util
//!     .build()?;
//! ```
//!
//! # Invariants
//! - JS names are explicit at every declaration site.
//! - Bodies exchange plain Rust data; the generated glue owns GC
//!   rooting, prototype linkage, and promise plumbing. Verify any new
//!   surface under `OTTER_GC_STRESS=1..16` (see the crate's tests).
//!
//! # See also
//! - docs site: *Declarative Bindings: Classes, Namespaces,
//!   Extensions* and *Embedding: Writing an Extension*.

// Binding crates resolve the macro-generated `::otter_vm::…` paths
// through this alias — the established linking convention.
extern crate otter_runtime as otter_vm;

use otter_macros::{HostClass, js_class, js_module, js_namespace, romp};
use otter_runtime::CapabilitySet;
use otter_runtime::marshal::{JsError, USVString, Uint8Array};

/// The class's backing data: a plain Rust struct. `Clone` is required
/// for by-value marshalling and async-method snapshots.
#[derive(Debug, Clone, HostClass)]
pub struct Counter {
    label: String,
    value: f64,
}

#[js_class(name = "Counter", feature = WEB, js = "counter.class.js")]
impl Counter {
    /// `new Counter(label, initial?)`. Returns the instance DATA; the
    /// engine allocates the object with the right prototype, so JS
    /// subclasses (`class Mine extends Counter`) work by construction.
    #[constructor]
    fn js_new(label: USVString, initial: Option<f64>) -> Counter {
        Counter {
            label: label.into_string(),
            value: initial.unwrap_or(0.0),
        }
    }

    #[getter(name = "label")]
    fn js_label(&self) -> String {
        self.label.clone()
    }

    #[getter(name = "value")]
    fn js_value(&self) -> f64 {
        self.value
    }

    /// `&mut self` = brand-checked mutable access to the host data.
    #[method(name = "increment", length = 0)]
    fn js_increment(&mut self, by: Option<f64>) -> f64 {
        self.value += by.unwrap_or(1.0);
        self.value
    }

    /// `async fn` = the promise protocol. Owned `self` snapshot; the
    /// future runs on the shared Tokio runtime; an immediately-ready
    /// body settles with no executor round-trip.
    #[method(name = "snapshotBytes")]
    async fn js_snapshot_bytes(self) -> Uint8Array {
        Uint8Array(self.label.into_bytes())
    }

    /// Statics are own data properties on the constructor. Returning
    /// the class mints a real branded instance — a natural factory.
    #[static_method(name = "fromValue")]
    fn js_from_value(label: USVString, value: f64) -> Counter {
        Counter {
            label: label.into_string(),
            value,
        }
    }
}

/// Marker type for the `Acme` global namespace.
pub struct Acme;

#[js_namespace(name = "Acme", feature = WEB, tag = "Acme")]
impl Acme {
    #[method(name = "version")]
    fn version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    #[method(name = "greet")]
    fn greet(name: USVString) -> Result<String, JsError> {
        if name.as_str().is_empty() {
            return Err(JsError::Type("name must not be empty".to_string()));
        }
        Ok(format!("hello, {}", name.as_str()))
    }
}

/// Marker type for the `acme:util` hosted module.
pub struct UtilModule;

#[js_module(prefix = "acme", name = "util", capabilities = true)]
impl UtilModule {
    #[export(name = "shout")]
    fn shout(text: USVString) -> String {
        text.as_str().to_uppercase()
    }

    /// `capabilities = true` + a leading `caps: &CapabilitySet`
    /// parameter = the install-time permission snapshot. Boolean
    /// gates read it here; argument-derived checks (path allowlists
    /// against a real argument) also belong in the body.
    #[export(name = "canReadEnv")]
    fn can_read_env(caps: &CapabilitySet, name: USVString) -> bool {
        caps.env_allows(name.as_str())
    }
}

romp! {
    name = "acme",
    ident = ACME_EXTENSION,
    classes = [CounterIntrinsic, AcmeIntrinsic],
    // Pure-JS members would go here as (source, defines = [...]) rows,
    // materialized natively on first touch of any defined name.
    js = [],
}
