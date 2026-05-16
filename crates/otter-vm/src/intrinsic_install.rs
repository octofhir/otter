//! Built-in installer trait and macro.
//!
//! Each JavaScript built-in class owns its full installation logic
//! inside its own module via the [`BuiltinIntrinsic`] trait. The
//! central [`crate::bootstrap`] table only orchestrates installation
//! order; it does not host any installer bodies. This keeps the
//! installer file small, makes the ownership of each surface obvious,
//! and lets a new built-in land by writing one per-class file plus
//! one entry in the bootstrap table.
//!
//! # Contents
//! - [`BuiltinIntrinsic`] — the trait every built-in installer
//!   implements.
//! - [`bootstrap_install_thunk`] — generic adapter turning
//!   `T::install` into a [`crate::bootstrap::BootstrapInstall`]
//!   function pointer through monomorphisation.
//! - [`bootstrap_entry!`] — declarative macro that produces a
//!   [`crate::bootstrap::BootstrapEntry`] for a given intrinsic type.
//!
//! # Invariants
//! - Implementations of [`BuiltinIntrinsic::install`] are responsible
//!   for the full surface of their class: constructor, prototype,
//!   prototype methods, static methods, constants, internal-slot
//!   defaults, prototype chaining. The installer must define the
//!   global binding so subsequent entries can resolve it.
//! - The thunk is `pub(crate)` so the macro can synthesize a
//!   `const`-eligible function pointer for the static
//!   [`crate::bootstrap::BOOTSTRAP_ENTRIES`] table.
//!
//! # Why a trait + macro
//! The `BootstrapInstall` function-pointer signature predates this
//! split. Closures with captures cannot live in a `static` array, so
//! the macro routes through a generic free function. Monomorphisation
//! gives each intrinsic type its own concrete `fn` pointer, preserving
//! the deterministic table layout while letting installer bodies live
//! anywhere in the crate.
//!
//! # See also
//! - [`crate::bootstrap`] — registry orchestration and telemetry.
//! - [`crate::string::intrinsic`] — first migrated user.

use crate::bootstrap::{BootstrapEntry, BootstrapFeatures};
use crate::js_surface::JsSurfaceError;
use crate::object::JsObject;

/// One JavaScript built-in class' installation contract.
///
/// Implementors live next to the rest of the class' implementation
/// (e.g. `crate::string::intrinsic::Intrinsic`) so that everything
/// touching the class is co-located. The bootstrap registry refers
/// to the implementor by type via the [`bootstrap_entry!`] macro.
pub trait BuiltinIntrinsic {
    /// Global property name installed by this intrinsic.
    const NAME: &'static str;
    /// Feature/capability bits required at install time.
    const FEATURE: BootstrapFeatures;

    /// Install the full surface (constructor, prototype, statics,
    /// constants, global binding) onto `global`.
    ///
    /// The implementation owns the entire installation sequence. It
    /// must define the matching global property before returning so
    /// later bootstrap entries can resolve it through ordinary
    /// property lookup.
    ///
    /// # Errors
    /// - [`JsSurfaceError`] propagates allocation failures, attribute
    ///   conflicts, or builder misuse. The installer is expected to
    ///   surface OOM as [`JsSurfaceError::OutOfMemory`] rather than
    ///   panic.
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError>;
}

/// Generic adapter that lets the bootstrap function-pointer table
/// reference any [`BuiltinIntrinsic`] implementor.
///
/// Monomorphisation produces a unique `fn(&BootstrapEntry, &mut
/// GcHeap, JsObject) -> Result<(), JsSurfaceError>` pointer for each
/// `T`, suitable for storing inside the static [`crate::bootstrap::BOOTSTRAP_ENTRIES`].
pub(crate) fn bootstrap_install_thunk<T: BuiltinIntrinsic>(
    _entry: &BootstrapEntry,
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
) -> Result<(), JsSurfaceError> {
    T::install(heap, global)
}

/// Build a [`crate::bootstrap::BootstrapEntry`] for a type
/// implementing [`BuiltinIntrinsic`].
///
/// ```ignore
/// use crate::bootstrap_entry;
///
/// pub static BOOTSTRAP_ENTRIES: &[BootstrapEntry] = &[
///     bootstrap_entry!(crate::string::intrinsic::Intrinsic),
///     // ...
/// ];
/// ```
#[macro_export]
macro_rules! bootstrap_entry {
    ($intrinsic:path) => {
        $crate::bootstrap::BootstrapEntry {
            name: <$intrinsic as $crate::intrinsic_install::BuiltinIntrinsic>::NAME,
            feature: <$intrinsic as $crate::intrinsic_install::BuiltinIntrinsic>::FEATURE,
            install: $crate::intrinsic_install::bootstrap_install_thunk::<$intrinsic>,
        }
    };
}
