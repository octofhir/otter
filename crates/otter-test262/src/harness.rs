//! Test262 harness implementation
//!
//! Provides the $262 object and other harness functions required by Test262.

use otter_vm_core::object::PropertyKey;
use otter_vm_core::{JsObject, Value, VmContext};
use std::sync::Arc;

/// Set up the Test262 harness on a context
pub fn setup_harness(ctx: &mut VmContext) {
    let global = ctx.global();

    // Create $262 object
    let obj_262 = Arc::new(JsObject::new(None));

    // $262.createRealm() - Create a new realm (not fully implemented)
    // $262.detachArrayBuffer() - Detach an ArrayBuffer
    // $262.evalScript() - Evaluate a script
    // $262.gc() - Trigger garbage collection
    // $262.global - Reference to the global object
    // $262.agent - Agent-related functionality

    global.set(PropertyKey::string("$262"), Value::object(obj_262));

    // Set up print function (for test output)
    // In a real implementation, this would be a native function
    // For now, we just create a placeholder

    // Set up $DONE for async tests
    // async tests call $DONE() or $DONE(error) when complete

    // Set up assert functions
    setup_assert(global);
}

/// Set up assert helpers
fn setup_assert(global: &Arc<JsObject>) {
    let assert_obj = Arc::new(JsObject::new(None));

    // assert(condition, message) - Basic assertion
    // assert.sameValue(actual, expected, message) - Strict equality assertion
    // assert.notSameValue(actual, expected, message) - Strict inequality assertion
    // assert.throws(errorType, fn, message) - Exception assertion

    global.set(PropertyKey::string("assert"), Value::object(assert_obj));
}

/// Standard harness files content
pub struct HarnessFiles {
    /// assert.js content
    pub assert: &'static str,
    /// sta.js content (standard test assertions)
    pub sta: &'static str,
    /// doneprintHandle.js for async tests
    pub done_print_handle: &'static str,
}

impl Default for HarnessFiles {
    fn default() -> Self {
        Self::new()
    }
}

impl HarnessFiles {
    /// Create harness files with embedded content
    pub fn new() -> Self {
        Self {
            assert: include_str!("harness/assert.js"),
            sta: include_str!("harness/sta.js"),
            done_print_handle: include_str!("harness/donePrintHandle.js"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_harness_setup() {
        let global = Arc::new(JsObject::new(None));
        let mut ctx = VmContext::new(global.clone());

        setup_harness(&mut ctx);

        // Check $262 exists
        assert!(ctx.global().has(&PropertyKey::string("$262")));

        // Check assert exists
        assert!(ctx.global().has(&PropertyKey::string("assert")));
    }
}
