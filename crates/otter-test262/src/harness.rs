use otter_engine::{
    Extension, GcRef, JsObject, Op, OpHandler, PropertyKey, Value, VmContext, VmError,
};
use std::sync::{Arc, Mutex};

/// JS bootstrap executed by the extension to set up `print`, `$262`, etc.
/// Runs BEFORE any test262 harness files (sta.js, assert.js) are prepended.
/// Uses native ops `__test262_print`, `__test262_done`, `__otter_eval`.
const HARNESS_SETUP_JS: &str = r#"
// print() - routes to native __test262_print for output capture
var print = function() {
    for (var i = 0; i < arguments.length; i++) {
        __test262_print(arguments[i]);
    }
};

// $DONE() - async test completion handler, routes to native __test262_done
var $DONE = function(err) {
    if (err) {
        __test262_done(err);
    } else {
        __test262_done();
    }
};

// $262 host object (test262 host-defined)
var $262 = {
    global: this,
    gc: function() {
        if (typeof __test262_gc === 'function') {
            __test262_gc();
        }
    },
    evalScript: function(code) {
        // Use indirect eval to execute in global scope (Test262 requirement)
        // The (1, eval) pattern makes eval execute in global scope, not local
        return (1, eval)(code);
    },
    detachArrayBuffer: function(buffer) {
        if (typeof __test262_detach_array_buffer === 'function') {
            __test262_detach_array_buffer(buffer);
        }
    },
    createRealm: function() {
        if (typeof __otter_create_realm === 'function') {
            return __otter_create_realm();
        }
        // Fallback shim when real realms are not available.
        var parentGlobal = this.global;
        var realmGlobal = Object.create(parentGlobal);
        var ParentSymbol = parentGlobal.Symbol;

        function SymbolWrapper(description) {
            if (new.target) {
                throw new TypeError('Symbol is not a constructor');
            }
            return ParentSymbol(description);
        }

        // Share Symbol.prototype across realms for now.
        SymbolWrapper.prototype = ParentSymbol.prototype;

        // Copy well-known symbols (shared across realms per spec).
        SymbolWrapper.iterator = ParentSymbol.iterator;
        SymbolWrapper.asyncIterator = ParentSymbol.asyncIterator;
        SymbolWrapper.toStringTag = ParentSymbol.toStringTag;
        SymbolWrapper.hasInstance = ParentSymbol.hasInstance;
        SymbolWrapper.toPrimitive = ParentSymbol.toPrimitive;
        SymbolWrapper.isConcatSpreadable = ParentSymbol.isConcatSpreadable;
        SymbolWrapper.match = ParentSymbol.match;
        SymbolWrapper.matchAll = ParentSymbol.matchAll;
        SymbolWrapper.replace = ParentSymbol.replace;
        SymbolWrapper.search = ParentSymbol.search;
        SymbolWrapper.split = ParentSymbol.split;
        SymbolWrapper.species = ParentSymbol.species;
        SymbolWrapper.unscopables = ParentSymbol.unscopables;

        // Wrap registry accessors to keep behavior but ensure function identity differs.
        SymbolWrapper.for = function(key) { return ParentSymbol.for(key); };
        SymbolWrapper.keyFor = function(sym) { return ParentSymbol.keyFor(sym); };

        realmGlobal.Symbol = SymbolWrapper;

        return {
            global: realmGlobal,
            evalScript: function(code) {
                return $262.evalScript(code);
            }
        };
    },
    agent: {
        start: function(script) {
            // Stub: agent worker threads not yet supported
            throw new Error('$262.agent.start not yet implemented');
        },
        broadcast: function(buffer) {
            throw new Error('$262.agent.broadcast not yet implemented');
        },
        getReport: function() {
            return null;
        },
        sleep: function(ms) {
            // Busy-wait approximation (no real thread sleep in JS)
            var end = Date.now() + ms;
            while (Date.now() < end) {}
        },
        monotonicNow: function() {
            return Date.now();
        }
    }
};
"#;

/// Shared state for capturing async test results and print output.
///
/// Created once at engine build time, shared via `Arc<Mutex<...>>` with
/// the native op closures. The runner clears it before each test and
/// reads it after `eval()` completes.
#[derive(Debug, Clone)]
pub struct TestHarnessState {
    inner: Arc<Mutex<TestHarnessInner>>,
}

#[derive(Debug, Default)]
struct TestHarnessInner {
    /// Captured print output lines
    print_output: Vec<String>,
    /// Result from $DONE: None = not called, Some(Ok(())) = pass, Some(Err(msg)) = fail
    done_result: Option<Result<(), String>>,
}

impl Default for TestHarnessState {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(TestHarnessInner::default())),
        }
    }
}

impl TestHarnessState {
    /// Create a new shared harness state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Clear state before running a new test.
    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap();
        inner.print_output.clear();
        inner.done_result = None;
    }

    /// Check whether `$DONE` was called. Returns:
    /// - `None` if `$DONE` was never called
    /// - `Some(Ok(()))` if `$DONE()` was called with no error
    /// - `Some(Err(msg))` if `$DONE(error)` was called
    pub fn done_result(&self) -> Option<Result<(), String>> {
        self.inner.lock().unwrap().done_result.clone()
    }

    /// Get all captured print output lines.
    pub fn print_output(&self) -> Vec<String> {
        self.inner.lock().unwrap().print_output.clone()
    }
}

/// Create a Test262 harness extension with shared state for output capture.
///
/// The returned `TestHarnessState` handle should be stored on the runner
/// and used to inspect async test results after each `eval()`.
pub fn create_harness_extension_with_state() -> (Extension, TestHarnessState) {
    let state = TestHarnessState::new();

    let print_state = state.inner.clone();
    let done_state = state.inner.clone();

    let ext = Extension::new("test262")
        .with_ops(vec![
            otter_engine::op_native("__test262_print", move |args| {
                let mut inner = print_state.lock().unwrap();
                for arg in args {
                    let line = format_value(arg);
                    inner.print_output.push(line);
                }
                Ok(Value::undefined())
            }),
            otter_engine::op_native("__test262_done", move |args| {
                let mut inner = done_state.lock().unwrap();
                if let Some(err) = args.first()
                    && !err.is_undefined()
                    && !err.is_null()
                {
                    let msg = format_value(err);
                    inner.done_result = Some(Err(msg));
                    // Don't return Err — that would abort the VM.
                    // Instead, store the failure for the runner to read.
                    return Ok(Value::undefined());
                }
                inner.done_result = Some(Ok(()));
                Ok(Value::undefined())
            }),
            // $262.gc() - triggers garbage collection via MemoryManager
            Op {
                name: "__test262_gc".into(),
                handler: OpHandler::Native(Arc::new(|_args, memory_manager| {
                    memory_manager.request_gc();
                    Ok(Value::undefined())
                })),
            },
            // $262.detachArrayBuffer() - detaches an ArrayBuffer
            otter_engine::op_native("__test262_detach_array_buffer", |args| {
                if let Some(buffer) = args.first() {
                    if buffer.is_array_buffer() {
                        if let Some(array_buffer) = buffer.as_array_buffer() {
                            // Use the proper ArrayBuffer detach API
                            array_buffer.detach();
                            return Ok(Value::undefined());
                        }
                    }
                }
                Err(VmError::type_error(
                    "detachArrayBuffer requires an ArrayBuffer",
                ))
            }),
        ])
        .with_js(HARNESS_SETUP_JS);

    (ext, state)
}

/// Create a Test262 harness extension (legacy, without state capture).
pub fn create_harness_extension() -> Extension {
    create_harness_extension_with_state().0
}

/// Set up the Test262 harness on a context.
///
/// **Deprecated**: Use `create_harness_extension_with_state()` with the extension system instead.
/// This legacy function does not capture async test results via `TestHarnessState`.
#[deprecated(note = "Use create_harness_extension_with_state() with the extension system instead")]
pub fn setup_harness(ctx: &mut VmContext) {
    let global = ctx.global();
    let mm = Arc::clone(global.memory_manager());

    // Create $262 object
    let obj_262 = GcRef::new(JsObject::new(Value::null(), Arc::clone(&mm)));

    // $262.global - Reference to the global object
    let _ = obj_262.set(PropertyKey::string("global"), Value::object(global));

    // $262.gc() - Trigger garbage collection
    let _ = obj_262.set(
        PropertyKey::string("gc"),
        Value::native_function(
            |_this, _args, _mm| {
                // Trigger VM GC if supported
                Ok(Value::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    let _ = global.set(PropertyKey::string("$262"), Value::object(obj_262));

    // Set up print function (for test output)
    let _ = global.set(
        PropertyKey::string("print"),
        Value::native_function(
            |_this, args, _mm| {
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
    let _ = global.set(
        PropertyKey::string("$DONE"),
        Value::native_function(
            |_this: &Value, args: &[Value], _mm| {
                if let Some(err) = args.first()
                    && !err.is_undefined()
                    && !err.is_null()
                {
                    // Test failed
                    return Err(VmError::type_error(format!(
                        "Test failed via $DONE: {:?}",
                        err
                    )));
                }
                // Test passed
                Ok(Value::undefined())
            },
            Arc::clone(&mm),
        ),
    );

    // Note: assert object is created by assert.js harness file, not here.
    // This allows assert to be a function with methods (sameValue, throws, etc.)
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
    #[allow(deprecated)]
    fn test_harness_setup() {
        let runtime = VmRuntime::new();
        let mut ctx = runtime.create_context();

        setup_harness(&mut ctx);

        // Check $262 exists
        assert!(ctx.global().has(&PropertyKey::string("$262")));

        // Note: assert is created by assert.js harness file, not setup_harness
    }

    #[test]
    fn test_harness_state() {
        let state = TestHarnessState::new();
        assert!(state.done_result().is_none());
        assert!(state.print_output().is_empty());

        // Simulate $DONE() success
        {
            let mut inner = state.inner.lock().unwrap();
            inner.done_result = Some(Ok(()));
            inner.print_output.push("hello".to_string());
        }
        assert_eq!(state.done_result(), Some(Ok(())));
        assert_eq!(state.print_output(), vec!["hello".to_string()]);

        // Clear resets everything
        state.clear();
        assert!(state.done_result().is_none());
        assert!(state.print_output().is_empty());
    }
}
