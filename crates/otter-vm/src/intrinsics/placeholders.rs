//! Placeholder `BuiltinIntrinsic` adapters for `Intl`, `Temporal`, and
//! `AggregateError`. Each installs an empty object with a prototype
//! slot via [`crate::bootstrap::install_placeholder`]; the real spec
//! surfaces ship separately.

use crate::bootstrap::{BootstrapFeatures, install_placeholder};
use crate::js_surface::JsSurfaceError;
use crate::object::JsObject;

/// Placeholder `BuiltinIntrinsic` for `Intl`.
pub struct IntlIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for IntlIntrinsic {
    const NAME: &'static str = "Intl";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}

/// Placeholder `BuiltinIntrinsic` for `Temporal`.
pub struct TemporalIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for TemporalIntrinsic {
    const NAME: &'static str = "Temporal";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}

/// Placeholder `BuiltinIntrinsic` for `AggregateError`.
pub struct AggregateErrorIntrinsic;
impl crate::intrinsic_install::BuiltinIntrinsic for AggregateErrorIntrinsic {
    const NAME: &'static str = "AggregateError";
    const FEATURE: BootstrapFeatures = BootstrapFeatures::CORE;
    fn install(heap: &mut otter_gc::GcHeap, global: JsObject) -> Result<(), JsSurfaceError> {
        install_placeholder(Self::NAME, heap, global)
    }
}
