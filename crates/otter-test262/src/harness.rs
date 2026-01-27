use otter_engine::{Extension, JsObject, PropertyKey, Value, VmContext};
use std::sync::Arc;

/// Create a Test262 harness extension
pub fn create_harness_extension() -> Extension {
    Extension::new("test262")
        .with_js(include_str!("harness/sta.js"))
        .with_js(include_str!("harness/assert.js"))
        .with_js(include_str!("harness/donePrintHandle.js"))
        .with_ops(vec![
            otter_engine::op_native("__test262_print", |args| {
                for arg in args {
                    println!("{}", format_value(arg));
                }
                Ok(Value::undefined())
            }),
            otter_engine::op_native("__test262_done", |args| {
                if let Some(err) = args.first() {
                    if !err.is_undefined() && !err.is_null() {
                        return Err(format!("Test failed via $DONE: {:?}", err));
                    }
                }
                Ok(Value::undefined())
            }),
        ])
}

/// Set up the Test262 harness on a context
pub fn setup_harness(ctx: &mut VmContext) {
    let global = ctx.global();
    let mm = Arc::clone(global.memory_manager());

    // Create $262 object
    let obj_262 = Arc::new(JsObject::new(None, Arc::clone(&mm)));

    // $262.global - Reference to the global object
    obj_262.set(PropertyKey::string("global"), Value::object(global.clone()));

    // $262.gc() - Trigger garbage collection
    obj_262.set(
        PropertyKey::string("gc"),
        Value::native_function(
            |_args, _mm| {
            // Trigger VM GC if supported
            Ok(Value::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    global.set(PropertyKey::string("$262"), Value::object(obj_262));

    // Set up print function (for test output)
    global.set(
        PropertyKey::string("print"),
        Value::native_function(
            |args, _mm| {
                for arg in args {
                    println!("{}", format_value(arg));
                }
                Ok(Value::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    // Set up $DONE for async tests
    // async tests call $DONE() or $DONE(error) when complete
    global.set(
        PropertyKey::string("$DONE"),
        Value::native_function(
            |args, _mm| {
                if let Some(err) = args.first() {
                    if !err.is_undefined() && !err.is_null() {
                        // Test failed
                        return Err(format!("Test failed via $DONE: {:?}", err));
                    }
                }
                // Test passed
                Ok(Value::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    // Set up assert functions
    setup_assert(global);
}

fn format_value(value: &Value) -> String {
    if value.is_undefined() {
        return "undefined".to_string();
    }
    if value.is_null() {
        return "null".to_string();
    }
    if let Some(s) = value.as_string() {
        return s.as_str().to_string();
    }
    format!("{:?}", value)
}

/// Set up assert helpers
fn setup_assert(global: &Arc<JsObject>) {
    let mm = Arc::clone(global.memory_manager());
    let assert_obj = Arc::new(JsObject::new(None, mm));

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
    use otter_engine::VmRuntime;

    #[test]
    fn test_harness_setup() {
        let runtime = VmRuntime::new();
        let mut ctx = runtime.create_context();

        setup_harness(&mut ctx);

        // Check $262 exists
        assert!(ctx.global().has(&PropertyKey::string("$262")));

        // Check assert exists
        assert!(ctx.global().has(&PropertyKey::string("assert")));
    }
}
