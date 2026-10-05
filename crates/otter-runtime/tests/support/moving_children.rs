//! Exact young-child motion through public native handle scopes.
//!
//! # Contents
//! - Native entries for nonallocating offset observations and real collection.
//! - Panic-free owned observations of each child's relocation and payload.
//! - A checking wrapper for fixtures that assert during their native callback.
//!
//! # Invariants
//! - Children remain in collector-rewritten handles throughout collection.
//! - Only owned scalar offsets and markers survive a collecting operation.
//! - The observer borrows a live native argument without allocating or retaining it.
//! - Old space is nonmoving: an actual offset change proves young evacuation,
//!   without inspecting raw headers or assuming a freshly allocated cell's age.
//!
//! # See also
//! - `jit_actual_only_calls` covers children reached through actual bound payloads.
//! - `jit_persistent_inline_fields` covers resident and overflow bank children.

use std::sync::Arc;

use otter_runtime::{
    OtterError, RuntimeExtensionContext, RuntimeNativeCall, RuntimeNativeCtx, RuntimeNativeError,
    RuntimeValue,
};

const OBSERVE: &str = "motionChildOffset";
const COLLECT: &str = "motionCollect";

#[derive(Debug)]
pub(super) struct ChildMotion {
    pub(super) before: u32,
    pub(super) after: u32,
    pub(super) marker_before: f64,
    pub(super) marker_after: f64,
}

pub(super) fn install(realm: &mut RuntimeExtensionContext<'_>) -> Result<(), OtterError> {
    realm.install_native_global_call(
        OBSERVE,
        1,
        RuntimeNativeCall::Dynamic(Arc::new(
            |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _state: &[RuntimeValue]| {
                let child = args
                    .first()
                    .copied()
                    .and_then(RuntimeValue::as_object)
                    .ok_or_else(|| RuntimeNativeError::Error {
                        message: "offset observation requires a live ordinary child".into(),
                    })?;
                Ok(RuntimeValue::number_i32(child.offset() as i32))
            },
        )),
    )?;
    realm.install_native_global_call(
        COLLECT,
        0,
        RuntimeNativeCall::Dynamic(Arc::new(
            |ctx: &mut RuntimeNativeCtx<'_>, _args: &[RuntimeValue], _state: &[RuntimeValue]| {
                ctx.interp_mut()
                    .force_gc()
                    .map_err(|error| RuntimeNativeError::Error {
                        message: format!("full collection with scoped children: {error}"),
                    })?;
                Ok(RuntimeValue::undefined())
            },
        )),
    )
}

pub(super) fn observe_and_collect(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<Vec<ChildMotion>, RuntimeNativeError> {
    ctx.scope(|mut scope| {
        let children: Vec<_> = (0..args.len())
            .map(|index| scope.argument(args, index))
            .collect();
        let observer = scope
            .global(OBSERVE)
            .ok_or_else(|| RuntimeNativeError::Error {
                message: "missing native child offset observer".into(),
            })?;
        let collector = scope
            .global(COLLECT)
            .ok_or_else(|| RuntimeNativeError::Error {
                message: "missing native scoped child collector".into(),
            })?;
        let receiver = scope.undefined();
        let mut before = Vec::with_capacity(children.len());
        let mut markers = Vec::with_capacity(children.len());
        for &child in &children {
            let offset = scope.call(observer, receiver, &[child])?;
            before.push(scope.number_value(offset)? as i32 as u32);
            let marker = scope.get(child, "marker")?;
            markers.push(scope.number_value(marker)?);
        }
        scope.call(collector, receiver, &[])?;
        let mut observations = Vec::with_capacity(children.len());
        for (index, &child) in children.iter().enumerate() {
            let offset = scope.call(observer, receiver, &[child])?;
            let after = scope.number_value(offset)? as i32 as u32;
            let marker = scope.get(child, "marker")?;
            observations.push(ChildMotion {
                before: before[index],
                after,
                marker_before: markers[index],
                marker_after: scope.number_value(marker)?,
            });
        }
        Ok(observations)
    })
}

// Shared by test crates that retain the checking callback; the persistent-field
// fixture checks the owned observations only after returning to Rust instead.
#[allow(dead_code)]
pub(super) fn collect_and_prove(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<(), RuntimeNativeError> {
    let observations = observe_and_collect(ctx, args)?;
    assert!(
        !observations.is_empty(),
        "a motion probe names its actual children"
    );
    for (index, child) in observations.iter().enumerate() {
        assert!(
            !observations[..index]
                .iter()
                .any(|prior| prior.before == child.before),
            "distinct field/actual children have distinct live cells"
        );
        assert_ne!(
            child.after, child.before,
            "the exact rooted child {index} must move"
        );
        assert_eq!(child.marker_after, child.marker_before);
    }
    Ok(())
}
