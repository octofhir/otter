//! Bounded aggregation of committed property runtime paths.
//!
//! # Contents
//! - Owned store-path diagnostics and batch-local event indices.
//! - Lazy counter insertion and in-place updates in the shared event buffer.
//!
//! # Invariants
//! - Disabled capture allocates nothing and never builds property names.
//! - Each distinct site/path/completion occupies one ordinary event slot.
//! - Existing counters continue counting after the event cap is reached;
//!   unseen keys count as dropped observations without growing the index.
//! - Indices are cleared whenever their owning event batch is drained/reset.
//! - No VM handles or addresses enter the keys or reports.

use serde::Serialize;

use super::{JIT_DEBUG_EVENT_LIMIT, JitDebugEvent, JitDebugState};

/// Path selected by one generated named-property store runtime entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum JitPropertyStorePath {
    /// The receiver is not represented by an ordinary object handle.
    NonObject,
    /// The object cannot use the ordinary fast-property IC.
    UnsupportedReceiver,
    /// An already-installed store recipe was probed.
    Cached,
    /// Canonical Set selected a setter, rejection, or another non-data path.
    SetSemantics,
    /// A writable existing-slot recipe was installed.
    InstallExisting,
    /// A new add-property recipe was captured.
    InstallTransition,
    /// Uncached ordinary data assignment, including saturated sites.
    UncachedData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct PropertyRuntimeKey {
    function_id: u32,
    instruction_pc: u32,
    path: JitPropertyStorePath,
    failed: bool,
    native_way: bool,
}

pub(super) type PropertyRuntimeIndices = rustc_hash::FxHashMap<PropertyRuntimeKey, usize>;

impl JitDebugState {
    /// Count one completed or failed runtime entry. `name` only runs when a
    /// previously unseen key can claim a slot in the current event batch.
    pub(crate) fn record_property_store(
        &mut self,
        function_id: u32,
        instruction_pc: u32,
        path: JitPropertyStorePath,
        failed: bool,
        native_way: bool,
        name: impl FnOnce() -> String,
    ) {
        let Some(events) = self.events.as_mut() else {
            return;
        };
        let key = PropertyRuntimeKey {
            function_id,
            instruction_pc,
            path,
            failed,
            native_way,
        };
        if let Some(&index) = self.property_runtime.get(&key) {
            let JitDebugEvent::PropertyStoreRuntime { count, .. } = &mut events[index] else {
                unreachable!("property counter index must belong to this batch");
            };
            *count = count.saturating_add(1);
            return;
        }
        if events.len() == JIT_DEBUG_EVENT_LIMIT {
            self.dropped_events = self.dropped_events.saturating_add(1);
            return;
        }
        let index = events.len();
        events.push(JitDebugEvent::PropertyStoreRuntime {
            function_id,
            instruction_pc,
            property_name: name(),
            path,
            failed,
            native_way,
            count: 1,
        });
        self.property_runtime.insert(key, index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jit_debug::JitDebugRequest;

    fn record(state: &mut JitDebugState, pc: u32, name: impl FnOnce() -> String) {
        state.record_property_store(7, pc, JitPropertyStorePath::Cached, false, false, name);
    }

    #[test]
    fn disabled_and_existing_keys_do_not_build_names() {
        let mut state = JitDebugState::default();
        record(&mut state, 3, || panic!("disabled capture"));
        assert!(state.property_runtime.is_empty());
        state.set_request(JitDebugRequest::events());
        record(&mut state, 3, || "x".to_string());
        record(&mut state, 3, || panic!("existing counter"));
        let report = state.take_report().unwrap();
        let json = serde_json::to_value(&report).expect("owned counter serialization");
        assert_eq!(
            json["events"][0],
            serde_json::json!({
                "type": "propertyStoreRuntime", "functionId": 7, "instructionPc": 3,
                "propertyName": "x", "path": "cached", "failed": false,
                "nativeWay": false, "count": 2
            })
        );
        assert!(matches!(
            report.events(),
            [JitDebugEvent::PropertyStoreRuntime { count: 2, .. }]
        ));
        record(&mut state, 3, || "new batch".to_string());
        assert!(matches!(
            state.take_report().unwrap().events(),
            [JitDebugEvent::PropertyStoreRuntime { count: 1, .. }]
        ));
    }

    #[test]
    fn full_batch_updates_known_keys_and_bounds_new_keys() {
        let mut state = JitDebugState::new(JitDebugRequest::events());
        for pc in 0..JIT_DEBUG_EVENT_LIMIT as u32 {
            record(&mut state, pc, || "x".to_string());
        }
        record(&mut state, 0, || panic!("known counter at cap"));
        record(&mut state, JIT_DEBUG_EVENT_LIMIT as u32, || {
            panic!("full batch")
        });
        assert_eq!(state.property_runtime.len(), JIT_DEBUG_EVENT_LIMIT);
        let report = state.take_report().unwrap();
        assert_eq!(report.dropped_events(), 1);
        assert!(matches!(
            report.events()[0],
            JitDebugEvent::PropertyStoreRuntime { count: 2, .. }
        ));
        state.begin_batch();
        record(&mut state, 0, || "fresh".to_string());
        state.begin_batch();
        assert!(state.property_runtime.is_empty());
        record(&mut state, 0, || "reset".to_string());
        state.set_request(JitDebugRequest::disabled());
        assert!(state.property_runtime.is_empty());
        assert!(state.take_report().is_none());
    }
}
