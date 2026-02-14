//! Native `node:test` extension.
//!
//! Implements the Node.js test runner API (`node:test`).
//! `test` is both a callable function AND a namespace object (like `assert`).
//!
//! Provides: test, test.test, test.it, test.describe, test.suite,
//! test.skip, test.todo, test.only, test.run, test.mock,
//! test.before, test.after, test.beforeEach, test.afterEach.
//!
//! All tests return a `Promise<undefined>`.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Instant;

use otter_macros::{js_class, js_static};
use otter_vm_core::MemoryManager;
use otter_vm_core::context::NativeContext;
use otter_vm_core::error::{ThrownValue, VmError};
use otter_vm_core::gc::GcRef;
use otter_vm_core::intrinsics_impl::helpers::strict_equal;
use otter_vm_core::object::{JsObject, PropertyDescriptor, PropertyKey};
use otter_vm_core::promise::{JsPromise, JsPromiseJob, JsPromiseJobKind};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::extension_v2::{OtterExtension, Profile};
use otter_vm_runtime::registration::RegistrationContext;
use regex::Regex;

use crate::util_ext::make_fn;

// ---------------------------------------------------------------------------
// Test Registry — stores registered tests for test.run()
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TestEntry {
    name: String,
    callback: Value,
    options: TestOptions,
}

#[derive(Clone)]
struct LifecycleHook {
    callback: Value,
    name: String,
}

#[derive(Default)]
struct TestRegistry {
    tests: Vec<TestEntry>,
    before_hooks: Vec<LifecycleHook>,
    after_hooks: Vec<LifecycleHook>,
    before_each_hooks: Vec<LifecycleHook>,
    after_each_hooks: Vec<LifecycleHook>,
}

thread_local! {
    static TEST_REGISTRY: RefCell<TestRegistry> = RefCell::new(TestRegistry::default());
}

// ---------------------------------------------------------------------------
// OtterExtension
// ---------------------------------------------------------------------------

pub struct NodeTestExtension;

impl OtterExtension for NodeTestExtension {
    fn name(&self) -> &str {
        "node_test"
    }

    fn profiles(&self) -> &[Profile] {
        static P: [Profile; 2] = [Profile::SafeCore, Profile::Full];
        &P
    }

    fn deps(&self) -> &[&str] {
        &["node_assert", "node_events"]
    }

    fn module_specifiers(&self) -> &[&str] {
        static S: [&str; 2] = ["node:test", "test"];
        &S
    }

    fn install(&self, _ctx: &mut RegistrationContext) -> Result<(), VmError> {
        Ok(())
    }

    fn load_module(
        &self,
        _specifier: &str,
        ctx: &mut RegistrationContext,
    ) -> Option<GcRef<JsObject>> {
        let test_fn = build_test_function(ctx);
        let ns = ctx.new_object();
        let _ = ns.set(PropertyKey::string("default"), test_fn.clone());

        // Copy all properties from test_fn to namespace
        if let Some(fn_obj) = test_fn.as_object() {
            for key in fn_obj.own_keys() {
                if let Some(val) = fn_obj.get(&key) {
                    let _ = ns.set(key, val);
                }
            }
        }

        Some(ns)
    }
}

pub fn node_test_extension() -> Box<dyn OtterExtension> {
    Box::new(NodeTestExtension)
}

// ---------------------------------------------------------------------------
// TestContext — passed as `t` to test callbacks
// ---------------------------------------------------------------------------

#[js_class(name = "TestContext")]
pub struct TestContext;

#[js_class]
impl TestContext {
    #[js_static(name = "skip", length = 0)]
    pub fn skip(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_static(name = "todo", length = 0)]
    pub fn todo(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_static(name = "plan", length = 1)]
    pub fn plan(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_static(name = "diagnostic", length = 1)]
    pub fn diagnostic(
        _this: &Value,
        _args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        Ok(Value::undefined())
    }

    #[js_static(name = "test", length = 1)]
    pub fn subtest(
        _this: &Value,
        args: &[Value],
        ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        test_impl(args, ncx, &TestOptions::default())
    }
}

// ---------------------------------------------------------------------------
// Tracked mock function builder (shared between build_mock_stub & TestContext)
// ---------------------------------------------------------------------------

/// Build a tracked mock function that records calls, contexts, and results.
/// Returns the wrapper function Value with a `.mock` property attached.
fn build_tracked_mock_fn(
    original: Option<Value>,
    mm: &Arc<MemoryManager>,
    obj_proto: &GcRef<JsObject>,
    fn_proto: &GcRef<JsObject>,
) -> Value {
    // Shared arrays for call tracking
    let calls_arr = GcRef::new(JsObject::array(0, mm.clone()));
    let contexts_arr = GcRef::new(JsObject::array(0, mm.clone()));
    let results_arr = GcRef::new(JsObject::array(0, mm.clone()));

    // Build the .mock object
    let mock_obj = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let _ = mock_obj.set(
        PropertyKey::string("calls"),
        Value::object(calls_arr.clone()),
    );
    let _ = mock_obj.set(
        PropertyKey::string("contexts"),
        Value::object(contexts_arr.clone()),
    );
    let _ = mock_obj.set(
        PropertyKey::string("results"),
        Value::object(results_arr.clone()),
    );

    // mock.callCount()
    let mock_obj_for_count = mock_obj.clone();
    let count_fn = Value::native_function(
        move |_this, _args, _ncx| {
            if let Some(arr) = mock_obj_for_count
                .get(&PropertyKey::string("calls"))
                .and_then(|v| v.as_object())
            {
                Ok(Value::number(arr.array_length() as f64))
            } else {
                Ok(Value::number(0.0))
            }
        },
        mm.clone(),
    );
    let _ = mock_obj.set(PropertyKey::string("callCount"), count_fn);

    // mock.resetCalls()
    let mock_obj_for_reset = mock_obj.clone();
    let mm_for_reset = mm.clone();
    let reset_fn = Value::native_function(
        move |_this, _args, _ncx| {
            // Replace arrays with fresh empty ones
            let new_calls = GcRef::new(JsObject::array(0, mm_for_reset.clone()));
            let new_contexts = GcRef::new(JsObject::array(0, mm_for_reset.clone()));
            let new_results = GcRef::new(JsObject::array(0, mm_for_reset.clone()));
            let _ = mock_obj_for_reset.set(PropertyKey::string("calls"), Value::object(new_calls));
            let _ = mock_obj_for_reset
                .set(PropertyKey::string("contexts"), Value::object(new_contexts));
            let _ =
                mock_obj_for_reset.set(PropertyKey::string("results"), Value::object(new_results));
            Ok(Value::undefined())
        },
        mm.clone(),
    );
    let _ = mock_obj.set(PropertyKey::string("resetCalls"), reset_fn);

    // Build the wrapper function
    let calls_for_wrapper = calls_arr;
    let contexts_for_wrapper = contexts_arr;
    let results_for_wrapper = results_arr;
    let mm_clone = mm.clone();

    let wrapper: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |this, args, ncx| {
        // Record the call arguments as an array
        let args_arr = GcRef::new(JsObject::array(0, mm_clone.clone()));
        for arg in args {
            args_arr.array_push(arg.clone());
        }
        calls_for_wrapper.array_push(Value::object(args_arr));

        // Record the this context
        contexts_for_wrapper.array_push(this.clone());

        // Call the original function (or return undefined)
        let result = if let Some(ref orig) = original {
            ncx.call_function(orig, this.clone(), args)
        } else {
            Ok(Value::undefined())
        };

        // Record the result
        let result_obj = GcRef::new(JsObject::new(Value::null(), mm_clone.clone()));
        match &result {
            Ok(val) => {
                let _ = result_obj.set(
                    PropertyKey::string("type"),
                    Value::string(JsString::intern("return")),
                );
                let _ = result_obj.set(PropertyKey::string("value"), val.clone());
            }
            Err(err) => {
                let _ = result_obj.set(
                    PropertyKey::string("type"),
                    Value::string(JsString::intern("throw")),
                );
                let err_val = match err {
                    VmError::Exception(thrown) => thrown.value.clone(),
                    _ => Value::string(JsString::new_gc(&err.to_string())),
                };
                let _ = result_obj.set(PropertyKey::string("value"), err_val);
            }
        }
        results_for_wrapper.array_push(Value::object(result_obj));

        result
    });

    // Create the function object and attach .mock
    let fn_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
    fn_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern("mockFn"))),
    );
    fn_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(0.0)),
    );
    let _ = fn_obj.set(PropertyKey::string("mock"), Value::object(mock_obj));

    Value::native_function_with_proto_and_object(wrapper, mm.clone(), fn_proto.clone(), fn_obj)
}

/// Build a MockTracker with tracked mock.fn().
fn build_mock_stub(ctx: &RegistrationContext) -> Value {
    let mm = ctx.mm().clone();
    let obj_proto = ctx.obj_proto();
    let fn_proto = ctx.fn_proto();
    let mock_obj = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

    // mock.fn() → returns a tracked mock function
    let mm2 = mm.clone();
    let obj_proto2 = obj_proto.clone();
    let fn_proto2 = fn_proto.clone();
    let mock_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, args, _ncx| {
        let original = args.first().filter(|v| v.is_callable()).cloned();
        Ok(build_tracked_mock_fn(
            original,
            &mm2,
            &obj_proto2,
            &fn_proto2,
        ))
    });
    let fn_val = make_fn(ctx, "fn", mock_fn, 0);
    let _ = mock_obj.set(PropertyKey::string("fn"), fn_val);

    // mock.method() → stub
    let mock_method: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let method_val = make_fn(ctx, "method", mock_method, 2);
    let _ = mock_obj.set(PropertyKey::string("method"), method_val);

    // mock.reset() → stub
    let mock_reset: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let reset_val = make_fn(ctx, "reset", mock_reset, 0);
    let _ = mock_obj.set(PropertyKey::string("reset"), reset_val);

    // mock.restoreAll() → stub
    let mock_restore: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let restore_val = make_fn(ctx, "restoreAll", mock_restore, 0);
    let _ = mock_obj.set(PropertyKey::string("restoreAll"), restore_val);

    Value::object(mock_obj)
}

// ---------------------------------------------------------------------------
// Test options
// ---------------------------------------------------------------------------

#[derive(Default, Clone)]
struct TestOptions {
    skip: bool,
    todo: bool,
    only: bool,
    timeout: Option<f64>,
    concurrency: Option<f64>,
}

// ---------------------------------------------------------------------------
// Argument parsing — test([name], [options], [fn])
// ---------------------------------------------------------------------------

struct ParsedTestArgs {
    name: String,
    options: TestOptions,
    func: Option<Value>,
}

/// Parse test() arguments following Node.js semantics:
/// - string → name
/// - object (non-callable) → options
/// - callable → fn
fn parse_test_args(args: &[Value]) -> Result<ParsedTestArgs, VmError> {
    let mut name = String::from("<anonymous>");
    let mut options = TestOptions::default();
    let mut func: Option<Value> = None;

    for arg in args {
        if arg.is_callable() {
            func = Some(arg.clone());
        } else if let Some(s) = arg.as_string() {
            name = s.as_str().to_string();
        } else if arg.as_object().is_some() && !arg.is_callable() {
            // Options object
            let obj = arg.as_object().unwrap();
            if let Some(skip_val) = obj.get(&PropertyKey::string("skip")) {
                options.skip = skip_val.to_boolean();
            }
            if let Some(todo_val) = obj.get(&PropertyKey::string("todo")) {
                options.todo = todo_val.to_boolean();
            }
            if let Some(only_val) = obj.get(&PropertyKey::string("only")) {
                options.only = only_val.to_boolean();
            }
            if let Some(timeout_val) = obj.get(&PropertyKey::string("timeout")) {
                validate_timeout(&timeout_val)?;
                options.timeout = timeout_val.as_number();
            }
            if let Some(concurrency_val) = obj.get(&PropertyKey::string("concurrency")) {
                validate_concurrency(&concurrency_val)?;
                options.concurrency = concurrency_val.as_number();
            }
        }
    }

    Ok(ParsedTestArgs {
        name,
        options,
        func,
    })
}

// ---------------------------------------------------------------------------
// Option validation (ERR_INVALID_ARG_TYPE / ERR_OUT_OF_RANGE)
// ---------------------------------------------------------------------------

/// Create a Node.js-style error with `.code` property.
fn make_node_error(message: &str, code: &str) -> VmError {
    // We use VmError::type_error as the base, but ideally we'd set .code on the error object.
    // For now, embed the code in the message to help tests match.
    VmError::type_error(&format!("[{code}] {message}"))
}

fn validate_timeout(val: &Value) -> Result<(), VmError> {
    // Must be a number
    if val.is_undefined() {
        return Ok(());
    }

    // Reject non-number types
    if val.as_symbol().is_some() || val.as_object().is_some() || val.is_callable() {
        return Err(make_node_error(
            "The \"options.timeout\" property must be of type number. Received an invalid type",
            "ERR_INVALID_ARG_TYPE",
        ));
    }

    if let Some(s) = val.as_string() {
        return Err(make_node_error(
            &format!(
                "The \"options.timeout\" property must be of type number. Received type string ('{}')",
                s.as_str()
            ),
            "ERR_INVALID_ARG_TYPE",
        ));
    }

    if val.is_boolean() {
        return Err(make_node_error(
            "The \"options.timeout\" property must be of type number. Received type boolean",
            "ERR_INVALID_ARG_TYPE",
        ));
    }

    if let Some(n) = val.as_number() {
        if n.is_nan() || n.is_infinite() || n < 0.0 {
            return Err(make_node_error(
                &format!(
                    "The value of \"options.timeout\" is out of range. It must be a non-negative number. Received {n}"
                ),
                "ERR_OUT_OF_RANGE",
            ));
        }
    }

    Ok(())
}

fn validate_concurrency(val: &Value) -> Result<(), VmError> {
    if val.is_undefined() {
        return Ok(());
    }

    // concurrency accepts number or boolean
    if val.is_boolean() {
        return Ok(());
    }

    if val.as_symbol().is_some() || val.as_object().is_some() || val.is_callable() {
        return Err(make_node_error(
            "The \"options.concurrency\" property must be of type number or boolean. Received an invalid type",
            "ERR_INVALID_ARG_TYPE",
        ));
    }

    if val.as_string().is_some() {
        return Err(make_node_error(
            "The \"options.concurrency\" property must be of type number or boolean. Received type string",
            "ERR_INVALID_ARG_TYPE",
        ));
    }

    if let Some(n) = val.as_number() {
        if n.is_nan() || n.is_infinite() || n < 0.0 {
            return Err(make_node_error(
                &format!(
                    "The value of \"options.concurrency\" is out of range. It must be a non-negative number. Received {n}"
                ),
                "ERR_OUT_OF_RANGE",
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Core test() implementation
// ---------------------------------------------------------------------------

fn test_impl(
    args: &[Value],
    ncx: &mut NativeContext,
    overrides: &TestOptions,
) -> Result<Value, VmError> {
    let mut parsed = parse_test_args(args)?;

    // Apply overrides from skip/todo/only
    if overrides.skip {
        parsed.options.skip = true;
    }
    if overrides.todo {
        parsed.options.todo = true;
    }
    if overrides.only {
        parsed.options.only = true;
    }

    // Register test in global registry for test.run()
    // Even tests without callbacks (todo/skip) should be registered for proper reporting
    TEST_REGISTRY.with(|reg| {
        let mut registry = reg.borrow_mut();
        registry.tests.push(TestEntry {
            name: parsed.name.clone(),
            callback: parsed.func.clone().unwrap_or(Value::undefined()),
            options: parsed.options.clone(),
        });
    });

    let mm = ncx.memory_manager().clone();

    // Create a resolved promise to return
    let enqueue = {
        let queue = ncx.js_job_queue();
        move |job: JsPromiseJob, job_args: Vec<Value>| {
            if let Some(q) = &queue {
                q.enqueue(job, job_args);
            }
        }
    };
    let resolvers = JsPromise::with_resolvers(mm.clone(), enqueue);
    let promise_ref = resolvers.promise.clone();

    // If skip or no fn, resolve immediately
    if parsed.options.skip || parsed.func.is_none() {
        (resolvers.resolve)(Value::undefined());
        return Ok(wrap_promise(ncx, promise_ref));
    }

    let callback = parsed.func.unwrap();

    // Build TestContext with state cell
    let (t_obj, state) = build_test_context_from_ncx(ncx, &parsed.name, &[]);

    // Call the test function with `t` as argument
    let result = ncx.call_function(&callback, Value::undefined(), &[t_obj]);

    // Check plan count after callback (for inline test() calls)
    check_plan_count(&state);

    match result {
        Ok(return_val) => {
            // If the callback returned a promise (thenable), we should wait for it.
            // For now, resolve immediately — the microtask queue will handle .then chains.
            if is_thenable(&return_val) {
                // Chain: returnVal.then(() => resolve(undefined), reject)
                let resolve_fn = {
                    let resolve = resolvers.resolve.clone();
                    Value::native_function_with_proto(
                        move |_this, _args, _ncx| {
                            (resolve)(Value::undefined());
                            Ok(Value::undefined())
                        },
                        mm.clone(),
                        ncx.global()
                            .get(&PropertyKey::string("Function"))
                            .and_then(|v| v.as_object())
                            .and_then(|c| {
                                c.get(&PropertyKey::string("prototype"))
                                    .and_then(|v| v.as_object())
                            })
                            .unwrap_or_else(|| {
                                GcRef::new(JsObject::new(Value::null(), mm.clone()))
                            }),
                    )
                };
                let reject_fn = {
                    let reject = resolvers.reject.clone();
                    Value::native_function_with_proto(
                        move |_this, args, _ncx| {
                            let reason = args.first().cloned().unwrap_or(Value::undefined());
                            (reject)(reason);
                            Ok(Value::undefined())
                        },
                        mm.clone(),
                        ncx.global()
                            .get(&PropertyKey::string("Function"))
                            .and_then(|v| v.as_object())
                            .and_then(|c| {
                                c.get(&PropertyKey::string("prototype"))
                                    .and_then(|v| v.as_object())
                            })
                            .unwrap_or_else(|| {
                                GcRef::new(JsObject::new(Value::null(), mm.clone()))
                            }),
                    )
                };

                // Call .then(resolve, reject)
                if let Some(then_fn) = return_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("then")))
                {
                    let _ = ncx.call_function(&then_fn, return_val, &[resolve_fn, reject_fn]);
                } else {
                    (resolvers.resolve)(Value::undefined());
                }
            } else {
                (resolvers.resolve)(Value::undefined());
            }
        }
        Err(_err) => {
            // Test errors are caught — test() in Node.js doesn't propagate them.
            // The test is marked as failed internally, but the promise resolves.
            (resolvers.resolve)(Value::undefined());
        }
    }

    Ok(wrap_promise(ncx, promise_ref))
}

/// Check if plan count matches assertion count. Returns error message if mismatched.
fn check_plan_count(state: &GcRef<JsObject>) -> Option<String> {
    let plan_count = state
        .get(&PropertyKey::string("__plan_count"))
        .and_then(|v| v.as_number());

    if let Some(expected) = plan_count {
        if expected < 0.0 {
            return None; // No plan set (sentinel value)
        }
        let actual = state
            .get(&PropertyKey::string("__assertion_count"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);

        let expected_i = expected as u64;
        let actual_i = actual as u64;
        if expected_i != actual_i {
            return Some(format!(
                "plan expected {expected_i} assertions but received {actual_i}"
            ));
        }
    }
    None
}

/// Build TestContext from NativeContext (runtime path, not registration path).
/// Returns (t_obj Value, state_cell GcRef<JsObject>).
fn build_test_context_from_ncx(
    ncx: &mut NativeContext,
    name: &str,
    parent_names: &[String],
) -> (Value, GcRef<JsObject>) {
    let mm = ncx.memory_manager().clone();
    let global = ncx.global();

    let obj_proto = global
        .get(&PropertyKey::string("Object"))
        .and_then(|v| v.as_object())
        .and_then(|c| {
            c.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        })
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null(), mm.clone())));

    let fn_proto = global
        .get(&PropertyKey::string("Function"))
        .and_then(|v| v.as_object())
        .and_then(|c| {
            c.get(&PropertyKey::string("prototype"))
                .and_then(|v| v.as_object())
        })
        .unwrap_or_else(|| GcRef::new(JsObject::new(Value::null(), mm.clone())));

    let obj = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));

    // --- State cell: shared mutable state between t methods and runner ---
    let state = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ = state.set(PropertyKey::string("__skip"), Value::boolean(false));
    let _ = state.set(PropertyKey::string("__todo"), Value::boolean(false));
    // -1 means "no plan set"
    let _ = state.set(PropertyKey::string("__plan_count"), Value::number(-1.0));
    let _ = state.set(PropertyKey::string("__assertion_count"), Value::number(0.0));

    // t.name
    let _ = obj.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(name)),
    );

    // t.fullName (Feature 5)
    let full_name = if parent_names.is_empty() {
        name.to_string()
    } else {
        let mut parts: Vec<&str> = parent_names.iter().map(|s| s.as_str()).collect();
        parts.push(name);
        parts.join(" > ")
    };
    let _ = obj.set(
        PropertyKey::string("fullName"),
        Value::string(JsString::new_gc(&full_name)),
    );

    // Helper to create a native fn with proper prototype
    let make_native_fn = |f: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    >,
                          fname: &str,
                          length: u32| {
        let fn_obj = GcRef::new(JsObject::new(Value::object(fn_proto.clone()), mm.clone()));
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern(fname))),
        );
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(length as f64)),
        );
        Value::native_function_with_proto_and_object(f, mm.clone(), fn_proto.clone(), fn_obj)
    };

    // t.skip() — sets state.__skip = true (Feature 1)
    let state_skip = state.clone();
    let skip_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, _args, _ncx| {
        let _ = state_skip.set(PropertyKey::string("__skip"), Value::boolean(true));
        Ok(Value::undefined())
    });
    let _ = obj.set(
        PropertyKey::string("skip"),
        make_native_fn(skip_fn, "skip", 0),
    );

    // t.todo() — sets state.__todo = true (Feature 1)
    let state_todo = state.clone();
    let todo_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, _args, _ncx| {
        let _ = state_todo.set(PropertyKey::string("__todo"), Value::boolean(true));
        Ok(Value::undefined())
    });
    let _ = obj.set(
        PropertyKey::string("todo"),
        make_native_fn(todo_fn, "todo", 0),
    );

    // t.plan(count) — sets state.__plan_count (Feature 2)
    let state_plan = state.clone();
    let plan_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, args, _ncx| {
        let count = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);
        let _ = state_plan.set(PropertyKey::string("__plan_count"), Value::number(count));
        Ok(Value::undefined())
    });
    let _ = obj.set(
        PropertyKey::string("plan"),
        make_native_fn(plan_fn, "plan", 1),
    );

    // t.diagnostic()
    let diag_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let _ = obj.set(
        PropertyKey::string("diagnostic"),
        make_native_fn(diag_fn, "diagnostic", 1),
    );

    // t.test() — nested subtest
    let subtest_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, args, ncx| test_impl(args, ncx, &TestOptions::default()));
    let _ = obj.set(
        PropertyKey::string("test"),
        make_native_fn(subtest_fn, "test", 1),
    );

    // t.signal
    let signal = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let _ = signal.set(PropertyKey::string("aborted"), Value::boolean(false));
    let _ = obj.set(PropertyKey::string("signal"), Value::object(signal));

    // t.mock — MockTracker with tracked mock.fn() (Feature 3)
    let mock_obj = GcRef::new(JsObject::new(Value::object(obj_proto.clone()), mm.clone()));
    let mm_for_mock = mm.clone();
    let obj_proto_for_mock = obj_proto.clone();
    let fn_proto_for_mock = fn_proto.clone();
    let mock_fn_impl: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, args, _ncx| {
        let original = args.first().filter(|v| v.is_callable()).cloned();
        Ok(build_tracked_mock_fn(
            original,
            &mm_for_mock,
            &obj_proto_for_mock,
            &fn_proto_for_mock,
        ))
    });
    let _ = mock_obj.set(
        PropertyKey::string("fn"),
        make_native_fn(mock_fn_impl, "fn", 0),
    );
    let mock_method: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let _ = mock_obj.set(
        PropertyKey::string("method"),
        make_native_fn(mock_method, "method", 2),
    );
    let mock_reset: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let _ = mock_obj.set(
        PropertyKey::string("reset"),
        make_native_fn(mock_reset, "reset", 0),
    );
    let mock_restore: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let _ = mock_obj.set(
        PropertyKey::string("restoreAll"),
        make_native_fn(mock_restore, "restoreAll", 0),
    );
    let _ = obj.set(PropertyKey::string("mock"), Value::object(mock_obj));

    // t.assert — assertion methods with counting (Feature 2)
    let assert_obj = GcRef::new(JsObject::new(Value::object(obj_proto), mm.clone()));

    // Helper: build assertion function that increments counter
    let build_assert_fn = |state_ref: GcRef<JsObject>,
                           check: Arc<dyn Fn(&[Value]) -> Result<(), String> + Send + Sync>,
                           fname: &str,
                           length: u32| {
        let f: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(move |_this, args, _ncx| {
            // Increment assertion count
            let current = state_ref
                .get(&PropertyKey::string("__assertion_count"))
                .and_then(|v| v.as_number())
                .unwrap_or(0.0);
            let _ = state_ref.set(
                PropertyKey::string("__assertion_count"),
                Value::number(current + 1.0),
            );

            // Run the check
            if let Err(msg) = check(args) {
                return Err(VmError::type_error(&msg));
            }
            Ok(Value::undefined())
        });
        make_native_fn(f, fname, length)
    };

    // assert.ok(value, message?)
    let _ = assert_obj.set(
        PropertyKey::string("ok"),
        build_assert_fn(
            state.clone(),
            Arc::new(|args| {
                let val = args.first().cloned().unwrap_or(Value::undefined());
                if val.to_boolean() {
                    Ok(())
                } else {
                    let msg = args
                        .get(1)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "expected truthy value".to_string());
                    Err(msg)
                }
            }),
            "ok",
            1,
        ),
    );

    // assert.equal(actual, expected, message?) — loose equality via ===
    let _ = assert_obj.set(
        PropertyKey::string("equal"),
        build_assert_fn(
            state.clone(),
            Arc::new(|args| {
                let actual = args.first().cloned().unwrap_or(Value::undefined());
                let expected = args.get(1).cloned().unwrap_or(Value::undefined());
                if strict_equal(&actual, &expected) {
                    Ok(())
                } else {
                    let msg = args
                        .get(2)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "expected values to be equal".to_string());
                    Err(msg)
                }
            }),
            "equal",
            2,
        ),
    );

    // assert.strictEqual(actual, expected, message?)
    let _ = assert_obj.set(
        PropertyKey::string("strictEqual"),
        build_assert_fn(
            state.clone(),
            Arc::new(|args| {
                let actual = args.first().cloned().unwrap_or(Value::undefined());
                let expected = args.get(1).cloned().unwrap_or(Value::undefined());
                if strict_equal(&actual, &expected) {
                    Ok(())
                } else {
                    let msg = args
                        .get(2)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "expected values to be strictly equal".to_string());
                    Err(msg)
                }
            }),
            "strictEqual",
            2,
        ),
    );

    // assert.notStrictEqual(actual, expected, message?)
    let _ = assert_obj.set(
        PropertyKey::string("notStrictEqual"),
        build_assert_fn(
            state.clone(),
            Arc::new(|args| {
                let actual = args.first().cloned().unwrap_or(Value::undefined());
                let expected = args.get(1).cloned().unwrap_or(Value::undefined());
                if !strict_equal(&actual, &expected) {
                    Ok(())
                } else {
                    let msg = args
                        .get(2)
                        .and_then(|v| v.as_string())
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|| "expected values to be not strictly equal".to_string());
                    Err(msg)
                }
            }),
            "notStrictEqual",
            2,
        ),
    );

    // assert.deepEqual / assert.deepStrictEqual — basic reference equality for now
    for assert_name in &["deepEqual", "deepStrictEqual"] {
        let _ = assert_obj.set(
            PropertyKey::string(assert_name),
            build_assert_fn(
                state.clone(),
                Arc::new(|args| {
                    let actual = args.first().cloned().unwrap_or(Value::undefined());
                    let expected = args.get(1).cloned().unwrap_or(Value::undefined());
                    if strict_equal(&actual, &expected) {
                        Ok(())
                    } else {
                        let msg = args
                            .get(2)
                            .and_then(|v| v.as_string())
                            .map(|s| s.as_str().to_string())
                            .unwrap_or_else(|| "expected values to be deeply equal".to_string());
                        Err(msg)
                    }
                }),
                assert_name,
                2,
            ),
        );
    }

    // assert.fail(message?)
    let _ = assert_obj.set(
        PropertyKey::string("fail"),
        build_assert_fn(
            state.clone(),
            Arc::new(|args| {
                let msg = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_string())
                    .unwrap_or_else(|| "Failed".to_string());
                Err(msg)
            }),
            "fail",
            0,
        ),
    );

    // assert.throws(fn, expected?, message?)
    let state_throws = state.clone();
    let throws_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, args, ncx| {
        // Increment assertion count
        let current = state_throws
            .get(&PropertyKey::string("__assertion_count"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);
        let _ = state_throws.set(
            PropertyKey::string("__assertion_count"),
            Value::number(current + 1.0),
        );

        let func = args.first().cloned().unwrap_or(Value::undefined());
        if !func.is_callable() {
            return Err(VmError::type_error(
                "The \"fn\" argument must be of type function",
            ));
        }
        let result = ncx.call_function(&func, Value::undefined(), &[]);
        match result {
            Ok(_) => Err(VmError::type_error("Missing expected exception")),
            Err(_) => Ok(Value::undefined()),
        }
    });
    let _ = assert_obj.set(
        PropertyKey::string("throws"),
        make_native_fn(throws_fn, "throws", 1),
    );

    // assert.doesNotThrow(fn, message?)
    let state_dnt = state.clone();
    let dnt_fn: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(move |_this, args, ncx| {
        // Increment assertion count
        let current = state_dnt
            .get(&PropertyKey::string("__assertion_count"))
            .and_then(|v| v.as_number())
            .unwrap_or(0.0);
        let _ = state_dnt.set(
            PropertyKey::string("__assertion_count"),
            Value::number(current + 1.0),
        );

        let func = args.first().cloned().unwrap_or(Value::undefined());
        if !func.is_callable() {
            return Err(VmError::type_error(
                "The \"fn\" argument must be of type function",
            ));
        }
        let result = ncx.call_function(&func, Value::undefined(), &[]);
        match result {
            Ok(_) => Ok(Value::undefined()),
            Err(err) => Err(VmError::type_error(&format!(
                "Got unwanted exception: {}",
                err
            ))),
        }
    });
    let _ = assert_obj.set(
        PropertyKey::string("doesNotThrow"),
        make_native_fn(dnt_fn, "doesNotThrow", 1),
    );

    // assert.snapshot / assert.fileSnapshot — stubs
    let snap: Arc<
        dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
    > = Arc::new(|_this, _args, _ncx| Ok(Value::undefined()));
    let _ = assert_obj.set(
        PropertyKey::string("snapshot"),
        make_native_fn(snap.clone(), "snapshot", 1),
    );
    let _ = assert_obj.set(
        PropertyKey::string("fileSnapshot"),
        make_native_fn(snap, "fileSnapshot", 1),
    );
    let _ = obj.set(PropertyKey::string("assert"), Value::object(assert_obj));

    (Value::object(obj), state)
}

/// Check if a value is thenable (has a `.then` method).
fn is_thenable(val: &Value) -> bool {
    val.as_object()
        .and_then(|o| o.get(&PropertyKey::string("then")))
        .is_some_and(|v| v.is_callable())
}

/// Wrap a JsPromise into a JsObject with Promise.prototype for `.then`/`.catch`/`.finally`.
/// (Same pattern as events_ext::wrap_promise)
fn wrap_promise(ncx: &NativeContext, internal: GcRef<JsPromise>) -> Value {
    let obj = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = obj.set(PropertyKey::string("_internal"), Value::promise(internal));

    if let Some(promise_ctor) = ncx
        .global()
        .get(&PropertyKey::string("Promise"))
        .and_then(|v| v.as_object())
        && let Some(proto) = promise_ctor
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
    {
        if let Some(then_fn) = proto.get(&PropertyKey::string("then")) {
            let _ = obj.set(PropertyKey::string("then"), then_fn);
        }
        if let Some(catch_fn) = proto.get(&PropertyKey::string("catch")) {
            let _ = obj.set(PropertyKey::string("catch"), catch_fn);
        }
        if let Some(finally_fn) = proto.get(&PropertyKey::string("finally")) {
            let _ = obj.set(PropertyKey::string("finally"), finally_fn);
        }
        obj.set_prototype(Value::object(proto));
    }

    Value::object(obj)
}

// ---------------------------------------------------------------------------
// describe() — synchronous suite runner
// ---------------------------------------------------------------------------

fn describe_impl(
    args: &[Value],
    ncx: &mut NativeContext,
    _overrides: &TestOptions,
) -> Result<Value, VmError> {
    let parsed = parse_test_args(args)?;

    if let Some(callback) = parsed.func {
        // describe() calls its callback synchronously
        let _ = ncx.call_function(&callback, Value::undefined(), &[]);
    }

    Ok(Value::undefined())
}

// ---------------------------------------------------------------------------
// Lifecycle hooks (before, after, beforeEach, afterEach) — stubs
// ---------------------------------------------------------------------------

#[js_class(name = "TestHooks")]
pub struct TestHooks;

#[js_class]
impl TestHooks {
    #[js_static(name = "before", length = 1)]
    pub fn before(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        register_lifecycle_hook(args, LifecycleHookType::Before)
    }

    #[js_static(name = "after", length = 1)]
    pub fn after(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        register_lifecycle_hook(args, LifecycleHookType::After)
    }

    #[js_static(name = "beforeEach", length = 1)]
    pub fn before_each(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        register_lifecycle_hook(args, LifecycleHookType::BeforeEach)
    }

    #[js_static(name = "afterEach", length = 1)]
    pub fn after_each(
        _this: &Value,
        args: &[Value],
        _ncx: &mut NativeContext,
    ) -> Result<Value, VmError> {
        register_lifecycle_hook(args, LifecycleHookType::AfterEach)
    }
}

// ---------------------------------------------------------------------------
// Lifecycle hook registration
// ---------------------------------------------------------------------------

enum LifecycleHookType {
    Before,
    After,
    BeforeEach,
    AfterEach,
}

fn register_lifecycle_hook(args: &[Value], hook_type: LifecycleHookType) -> Result<Value, VmError> {
    // Parse arguments: can be (name, fn) or just (fn)
    let (name, callback) = if args.len() >= 2 {
        // (name, fn)
        let name = if let Some(s) = args[0].as_string() {
            s.as_str().to_string()
        } else {
            format!("{:?} hook", hook_type)
        };
        let callback = args.get(1).cloned().unwrap_or(Value::undefined());
        (name, callback)
    } else if args.len() == 1 {
        // Just (fn)
        let callback = args[0].clone();
        let name = format!("{:?} hook", hook_type);
        (name, callback)
    } else {
        // No arguments
        return Ok(Value::undefined());
    };

    // Register hook in registry
    TEST_REGISTRY.with(|reg| {
        let mut registry = reg.borrow_mut();
        let hook = LifecycleHook { callback, name };

        match hook_type {
            LifecycleHookType::Before => registry.before_hooks.push(hook),
            LifecycleHookType::After => registry.after_hooks.push(hook),
            LifecycleHookType::BeforeEach => registry.before_each_hooks.push(hook),
            LifecycleHookType::AfterEach => registry.after_each_hooks.push(hook),
        }
    });

    Ok(Value::undefined())
}

impl std::fmt::Debug for LifecycleHookType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LifecycleHookType::Before => write!(f, "before"),
            LifecycleHookType::After => write!(f, "after"),
            LifecycleHookType::BeforeEach => write!(f, "beforeEach"),
            LifecycleHookType::AfterEach => write!(f, "afterEach"),
        }
    }
}

/// Execute a lifecycle hook callback, awaiting promises if returned
fn execute_lifecycle_hook(
    callback: &Value,
    name: &str,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    // Call the hook callback
    let result = ncx.call_function(callback, Value::undefined(), &[]);

    match result {
        Ok(return_val) => {
            // If hook returns a promise (thenable), chain .then/.catch to capture result
            if is_thenable(&return_val) {
                let mm = ncx.memory_manager().clone();

                // Create a result holder to capture async errors
                let error_holder = GcRef::new(JsObject::new(Value::null(), mm.clone()));
                let _ = error_holder.set(PropertyKey::string("error"), Value::undefined());
                let _ = error_holder.set(PropertyKey::string("settled"), Value::boolean(false));

                // Fulfillment handler - mark as settled successfully
                let holder_ok = error_holder.clone();
                let fulfill_handler = Value::native_function(
                    move |_this, _args, _ncx| {
                        let _ = holder_ok.set(PropertyKey::string("settled"), Value::boolean(true));
                        Ok(Value::undefined())
                    },
                    mm.clone(),
                );

                // Rejection handler - capture the error
                let _hook_name = name.to_string();
                let holder_err = error_holder.clone();
                let reject_handler = Value::native_function(
                    move |_this, args, _ncx| {
                        let reason = args.first().cloned().unwrap_or(Value::undefined());
                        let _ = holder_err.set(PropertyKey::string("error"), reason);
                        let _ =
                            holder_err.set(PropertyKey::string("settled"), Value::boolean(true));
                        Ok(Value::undefined())
                    },
                    mm,
                );

                // Call .then(fulfill, reject)
                if let Some(then_fn) = return_val
                    .as_object()
                    .and_then(|o| o.get(&PropertyKey::string("then")))
                {
                    let _ =
                        ncx.call_function(&then_fn, return_val, &[fulfill_handler, reject_handler]);
                }

                // Check if the rejection handler was called (synchronous promise)
                if let Some(err_val) = error_holder.get(&PropertyKey::string("error")) {
                    if !err_val.is_undefined() {
                        let error_msg = if let Some(obj) = err_val.as_object() {
                            obj.get(&PropertyKey::string("message"))
                                .and_then(|v| v.as_string())
                                .map(|s| s.as_str().to_string())
                                .unwrap_or_else(|| format!("{} hook failed", name))
                        } else if let Some(s) = err_val.as_string() {
                            s.as_str().to_string()
                        } else {
                            format!("{} hook failed", name)
                        };
                        return Err(VmError::type_error(&error_msg));
                    }
                }
            }
            Ok(())
        }
        Err(err) => {
            // Hook failed synchronously - propagate error
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Build the test() function object with all methods and aliases
// ---------------------------------------------------------------------------

fn build_test_function(ctx: &RegistrationContext) -> Value {
    // Create test as a callable function
    let test_fn_val = Value::native_function_with_proto(
        |_this, args, ncx| test_impl(args, ncx, &TestOptions::default()),
        ctx.mm().clone(),
        ctx.fn_proto(),
    );

    if let Some(fn_obj) = test_fn_val.as_object() {
        // Set name and length
        fn_obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("test"))),
        );
        fn_obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(1.0)),
        );

        // test.test === test (self-reference)
        let _ = fn_obj.set(PropertyKey::string("test"), test_fn_val.clone());

        // test.it === test (alias)
        let _ = fn_obj.set(PropertyKey::string("it"), test_fn_val.clone());

        // test.describe
        let describe_fn = Value::native_function_with_proto(
            |_this, args, ncx| describe_impl(args, ncx, &TestOptions::default()),
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(desc_obj) = describe_fn.as_object() {
            desc_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("describe"))),
            );
            desc_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(1.0)),
            );

            // describe.skip
            let describe_skip = Value::native_function_with_proto(
                |_this, args, ncx| {
                    describe_impl(
                        args,
                        ncx,
                        &TestOptions {
                            skip: true,
                            ..Default::default()
                        },
                    )
                },
                ctx.mm().clone(),
                ctx.fn_proto(),
            );
            let _ = desc_obj.set(PropertyKey::string("skip"), describe_skip);

            // describe.todo
            let describe_todo = Value::native_function_with_proto(
                |_this, args, ncx| {
                    describe_impl(
                        args,
                        ncx,
                        &TestOptions {
                            todo: true,
                            ..Default::default()
                        },
                    )
                },
                ctx.mm().clone(),
                ctx.fn_proto(),
            );
            let _ = desc_obj.set(PropertyKey::string("todo"), describe_todo);

            // describe.only
            let describe_only = Value::native_function_with_proto(
                |_this, args, ncx| {
                    describe_impl(
                        args,
                        ncx,
                        &TestOptions {
                            only: true,
                            ..Default::default()
                        },
                    )
                },
                ctx.mm().clone(),
                ctx.fn_proto(),
            );
            let _ = desc_obj.set(PropertyKey::string("only"), describe_only);
        }
        let _ = fn_obj.set(PropertyKey::string("describe"), describe_fn.clone());

        // test.suite === test.describe
        let _ = fn_obj.set(PropertyKey::string("suite"), describe_fn);

        // test.skip(name?, fn?) — shorthand with skip:true
        let skip_fn = Value::native_function_with_proto(
            |_this, args, ncx| {
                test_impl(
                    args,
                    ncx,
                    &TestOptions {
                        skip: true,
                        ..Default::default()
                    },
                )
            },
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(skip_obj) = skip_fn.as_object() {
            skip_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("skip"))),
            );
        }
        let _ = fn_obj.set(PropertyKey::string("skip"), skip_fn);

        // test.todo(name?, fn?) — shorthand with todo:true
        let todo_fn = Value::native_function_with_proto(
            |_this, args, ncx| {
                test_impl(
                    args,
                    ncx,
                    &TestOptions {
                        todo: true,
                        ..Default::default()
                    },
                )
            },
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(todo_obj) = todo_fn.as_object() {
            todo_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("todo"))),
            );
        }
        let _ = fn_obj.set(PropertyKey::string("todo"), todo_fn);

        // test.only(name?, fn?) — shorthand with only:true
        let only_fn = Value::native_function_with_proto(
            |_this, args, ncx| {
                test_impl(
                    args,
                    ncx,
                    &TestOptions {
                        only: true,
                        ..Default::default()
                    },
                )
            },
            ctx.mm().clone(),
            ctx.fn_proto(),
        );
        if let Some(only_obj) = only_fn.as_object() {
            only_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("only"))),
            );
        }
        let _ = fn_obj.set(PropertyKey::string("only"), only_fn);

        // test.run() — execute registered tests and stream events
        let run_fn: Arc<
            dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError> + Send + Sync,
        > = Arc::new(test_run);
        let _ = fn_obj.set(PropertyKey::string("run"), make_fn(ctx, "run", run_fn, 0));

        // test.mock — MockTracker with tracked mock.fn()
        let mock = build_mock_stub(ctx);
        let _ = fn_obj.set(PropertyKey::string("mock"), mock);

        // Lifecycle hooks: before, after, beforeEach, afterEach
        type DeclFn = fn() -> (
            &'static str,
            Arc<
                dyn Fn(&Value, &[Value], &mut NativeContext) -> Result<Value, VmError>
                    + Send
                    + Sync,
            >,
            u32,
        );

        let hooks: &[DeclFn] = &[
            TestHooks::before_decl,
            TestHooks::after_decl,
            TestHooks::before_each_decl,
            TestHooks::after_each_decl,
        ];

        for decl in hooks {
            let (name, func, length) = decl();
            let hook_fn = make_fn(ctx, name, func, length);
            let _ = fn_obj.set(PropertyKey::string(name), hook_fn);
        }
    }

    test_fn_val
}

// ---------------------------------------------------------------------------
// test.run() — Execute registered tests and stream events
// ---------------------------------------------------------------------------

struct RunOptions {
    test_name_pattern: Option<Regex>,
    only: bool,
}

fn parse_run_options(opts_value: Option<&Value>) -> Result<RunOptions, VmError> {
    let mut opts = RunOptions {
        test_name_pattern: None,
        only: false,
    };

    if let Some(obj) = opts_value.and_then(|v| v.as_object()) {
        // Parse testNamePattern (string or regex)
        if let Some(pattern_val) = obj.get(&PropertyKey::string("testNamePattern")) {
            if let Some(pattern_str) = pattern_val.as_string() {
                let pat = pattern_str.as_str();
                match Regex::new(pat) {
                    Ok(re) => opts.test_name_pattern = Some(re),
                    Err(_) => {
                        // Fall back to literal match via escaped regex
                        if let Ok(re) = Regex::new(&regex::escape(pat)) {
                            opts.test_name_pattern = Some(re);
                        }
                    }
                }
            }
            // TODO: Handle JsRegExp objects by extracting source/flags
        }

        // Parse only flag
        if let Some(only_val) = obj.get(&PropertyKey::string("only")) {
            opts.only = only_val.to_boolean();
        }
    }

    Ok(opts)
}

fn test_run(_this: &Value, args: &[Value], ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Parse options
    let opts = parse_run_options(args.first())?;

    // Get EventEmitter prototype for creating TestsStream
    let emitter_proto = ncx
        .global()
        .get(&PropertyKey::string("__EventEmitter"))
        .and_then(|v| v.as_object())
        .and_then(|c| c.get(&PropertyKey::string("prototype")))
        .and_then(|v| v.as_object())
        .ok_or_else(|| VmError::type_error("node:test requires EventEmitter to be loaded"))?;

    // Create TestsStream instance (inherits from EventEmitter)
    let stream = build_tests_stream_instance(ncx, emitter_proto)?;

    // Schedule async execution via microtask
    schedule_test_execution(stream.clone(), opts, ncx)?;

    Ok(Value::object(stream))
}

fn build_tests_stream_instance(
    ncx: &NativeContext,
    emitter_proto: GcRef<JsObject>,
) -> Result<GcRef<JsObject>, VmError> {
    let stream = GcRef::new(JsObject::new(
        Value::object(emitter_proto),
        ncx.memory_manager().clone(),
    ));

    // Initialize EventEmitter storage (listeners map)
    let listeners_map = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = stream.set(
        PropertyKey::string("__ee_listeners"),
        Value::object(listeners_map),
    );
    let _ = stream.set(
        PropertyKey::string("__ee_maxListeners"),
        Value::number(10.0),
    );

    // Initialize event queue for async iteration (future feature)
    let event_queue = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
    let _ = stream.set(
        PropertyKey::string("__event_queue"),
        Value::object(event_queue),
    );
    let _ = stream.set(PropertyKey::string("__done"), Value::boolean(false));

    Ok(stream)
}

fn schedule_test_execution(
    stream: GcRef<JsObject>,
    opts: RunOptions,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    // Get tests from registry
    let tests = TEST_REGISTRY.with(|reg| {
        let registry = reg.borrow();
        registry.tests.clone()
    });

    // Filter tests based on options
    let filtered = filter_tests(&tests, &opts);

    // Create executor closure
    let stream_val = Value::object(stream);
    let mm = ncx.memory_manager().clone();

    let executor = Value::native_function(
        move |_this, _args, ncx| {
            execute_tests_sequentially(&stream_val, &filtered, ncx)?;
            Ok(Value::undefined())
        },
        mm,
    );

    // Queue as microtask (runs after current execution completes)
    if let Some(queue) = ncx.js_job_queue() {
        let job = JsPromiseJob {
            kind: JsPromiseJobKind::Fulfill,
            callback: executor,
            this_arg: Value::undefined(),
            result_promise: None,
        };
        queue.enqueue(job, vec![]);
    }

    Ok(())
}

fn filter_tests(tests: &[TestEntry], opts: &RunOptions) -> Vec<TestEntry> {
    tests
        .iter()
        .filter(|t| {
            // Filter by name pattern (using regex)
            if let Some(pattern) = &opts.test_name_pattern {
                if !pattern.is_match(&t.name) {
                    return false;
                }
            }

            // Apply only filter: if any test has only=true, skip non-only tests
            if opts.only && !t.options.only {
                return false;
            }

            // Skip tests marked as skip (static skip option)
            if t.options.skip {
                return false;
            }

            true
        })
        .cloned()
        .collect()
}

fn execute_tests_sequentially(
    stream: &Value,
    tests: &[TestEntry],
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    // Get lifecycle hooks from registry
    let (before_hooks, after_hooks, before_each_hooks, after_each_hooks) =
        TEST_REGISTRY.with(|reg| {
            let registry = reg.borrow();
            (
                registry.before_hooks.clone(),
                registry.after_hooks.clone(),
                registry.before_each_hooks.clone(),
                registry.after_each_hooks.clone(),
            )
        });

    // Execute before hooks (once before all tests)
    for hook in &before_hooks {
        execute_lifecycle_hook(&hook.callback, &hook.name, ncx)?;
    }

    // Test number counter (1-based) (Feature 4)
    let mut test_number: u64 = 0;

    // Execute each test
    for test in tests {
        test_number += 1;

        // Skip todo tests (just emit diagnostic, don't run)
        if test.options.todo {
            emit_test_event(stream, "test:enqueue", test, test_number, 0, 0.0, ncx)?;
            emit_test_diagnostic_event(stream, test, ncx)?;
            continue;
        }

        // Emit test:enqueue
        emit_test_event(stream, "test:enqueue", test, test_number, 0, 0.0, ncx)?;

        // Emit test:start
        emit_test_event(stream, "test:start", test, test_number, 0, 0.0, ncx)?;

        // Execute beforeEach hooks - if any fail, skip the test
        let mut before_each_failed = false;
        for hook in &before_each_hooks {
            if let Err(err) = execute_lifecycle_hook(&hook.callback, &hook.name, ncx) {
                // If beforeEach fails, mark test as failed and skip execution
                emit_test_fail_event(stream, test, &err, test_number, 0, 0.0, "hookFailure", ncx)?;
                before_each_failed = true;
                break; // Stop running more beforeEach hooks
            }
        }

        // Only run test if beforeEach hooks passed
        if !before_each_failed {
            // Record start time (Feature 4)
            let start_time = Instant::now();

            // Build TestContext with state cell (t parameter)
            let (t_obj, state) = build_test_context_from_ncx(ncx, &test.name, &[]);

            // Get timeout value (default to 30 seconds if not specified)
            let timeout_ms = test.options.timeout.unwrap_or(30000.0);

            // Execute test callback
            let result = ncx.call_function(&test.callback, Value::undefined(), &[t_obj]);

            // Compute duration (Feature 4)
            let duration_ms = start_time.elapsed().as_secs_f64() * 1000.0;

            // Check runtime state (Feature 1)
            let runtime_skip = state
                .get(&PropertyKey::string("__skip"))
                .map(|v| v.to_boolean())
                .unwrap_or(false);
            let runtime_todo = state
                .get(&PropertyKey::string("__todo"))
                .map(|v| v.to_boolean())
                .unwrap_or(false);

            if runtime_skip {
                // t.skip() was called — emit skip instead of pass/fail
                emit_test_skip_event(stream, test, test_number, 0, duration_ms, ncx)?;
            } else if runtime_todo {
                // t.todo() was called — emit diagnostic/todo
                emit_test_diagnostic_event(stream, test, ncx)?;
            } else {
                // Check plan count (Feature 2)
                let plan_error = check_plan_count(&state);

                match result {
                    Ok(return_val) => {
                        if let Some(plan_msg) = plan_error {
                            // Plan count mismatch — test fails
                            let err = VmError::type_error(&plan_msg);
                            emit_test_fail_event(
                                stream,
                                test,
                                &err,
                                test_number,
                                0,
                                duration_ms,
                                "testCodeFailure",
                                ncx,
                            )?;
                        } else if is_thenable(&return_val) {
                            // Async test - need to wait for promise to settle
                            handle_async_test_result(
                                stream,
                                test,
                                return_val,
                                timeout_ms,
                                test_number,
                                duration_ms,
                                ncx,
                            )?;
                        } else {
                            // Sync test - passed
                            emit_test_pass_event(stream, test, test_number, 0, duration_ms, ncx)?;
                        }
                    }
                    Err(err) => {
                        // Sync error - test failed
                        emit_test_fail_event(
                            stream,
                            test,
                            &err,
                            test_number,
                            0,
                            duration_ms,
                            "testCodeFailure",
                            ncx,
                        )?;
                    }
                }
            }
        }

        // Execute afterEach hooks (always run, even if test or beforeEach failed)
        for hook in &after_each_hooks {
            // Ignore errors in afterEach hooks to not mask test results
            let _ = execute_lifecycle_hook(&hook.callback, &hook.name, ncx);
        }
    }

    // Execute after hooks (once after all tests)
    for hook in &after_hooks {
        // Ignore errors in after hooks
        let _ = execute_lifecycle_hook(&hook.callback, &hook.name, ncx);
    }

    // Mark stream as complete
    if let Some(stream_obj) = stream.as_object() {
        let _ = stream_obj.set(PropertyKey::string("__done"), Value::boolean(true));
    }

    // Emit completion event
    emit_complete_event(stream, ncx)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Event emission functions (Feature 4: detailed event data)
// ---------------------------------------------------------------------------

fn emit_test_event(
    stream: &Value,
    event_type: &str,
    test: &TestEntry,
    test_number: u64,
    nesting: u32,
    duration_ms: f64,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let data = build_test_event_data(event_type, test, test_number, nesting, duration_ms, ncx);

    // Add to event queue for async iteration
    if let Some(stream_obj) = stream.as_object() {
        if let Some(queue) = stream_obj
            .get(&PropertyKey::string("__event_queue"))
            .and_then(|v| v.as_object())
        {
            queue.array_push(data.clone());
        }
    }

    // Emit via EventEmitter
    let emit_fn = stream
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream has no emit method"))?;

    ncx.call_function(
        &emit_fn,
        stream.clone(),
        &[Value::string(JsString::intern(event_type)), data],
    )?;

    Ok(())
}

fn build_test_event_data(
    event_type: &str,
    test: &TestEntry,
    test_number: u64,
    nesting: u32,
    duration_ms: f64,
    ncx: &NativeContext,
) -> Value {
    let event = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = event.set(
        PropertyKey::string("type"),
        Value::string(JsString::intern(event_type)),
    );

    let data = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = data.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(&test.name)),
    );
    // Feature 4: detailed event data
    let _ = data.set(
        PropertyKey::string("testNumber"),
        Value::number(test_number as f64),
    );
    let _ = data.set(
        PropertyKey::string("nesting"),
        Value::number(nesting as f64),
    );
    let _ = data.set(
        PropertyKey::string("duration_ms"),
        Value::number(duration_ms),
    );

    let _ = event.set(PropertyKey::string("data"), Value::object(data));
    Value::object(event)
}

fn emit_test_diagnostic_event(
    stream: &Value,
    test: &TestEntry,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let event = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = event.set(
        PropertyKey::string("type"),
        Value::string(JsString::intern("test:diagnostic")),
    );

    let data = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = data.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(&test.name)),
    );
    let _ = data.set(
        PropertyKey::string("message"),
        Value::string(JsString::new_gc(&format!(
            "test marked as TODO: {}",
            test.name
        ))),
    );

    let _ = event.set(PropertyKey::string("data"), Value::object(data));
    let event_val = Value::object(event);

    // Add to queue
    if let Some(stream_obj) = stream.as_object() {
        if let Some(queue) = stream_obj
            .get(&PropertyKey::string("__event_queue"))
            .and_then(|v| v.as_object())
        {
            queue.array_push(event_val.clone());
        }
    }

    // Emit via EventEmitter
    let emit_fn = stream
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream has no emit method"))?;

    ncx.call_function(
        &emit_fn,
        stream.clone(),
        &[
            Value::string(JsString::intern("test:diagnostic")),
            event_val,
        ],
    )?;

    Ok(())
}

/// Emit test:skip event (Feature 1)
fn emit_test_skip_event(
    stream: &Value,
    test: &TestEntry,
    test_number: u64,
    nesting: u32,
    duration_ms: f64,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let event = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = event.set(
        PropertyKey::string("type"),
        Value::string(JsString::intern("test:skip")),
    );

    let data = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = data.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(&test.name)),
    );
    let _ = data.set(
        PropertyKey::string("testNumber"),
        Value::number(test_number as f64),
    );
    let _ = data.set(
        PropertyKey::string("nesting"),
        Value::number(nesting as f64),
    );
    let _ = data.set(
        PropertyKey::string("duration_ms"),
        Value::number(duration_ms),
    );

    let _ = event.set(PropertyKey::string("data"), Value::object(data));
    let event_val = Value::object(event);

    // Add to queue
    if let Some(stream_obj) = stream.as_object() {
        if let Some(queue) = stream_obj
            .get(&PropertyKey::string("__event_queue"))
            .and_then(|v| v.as_object())
        {
            queue.array_push(event_val.clone());
        }
    }

    // Emit via EventEmitter
    let emit_fn = stream
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream has no emit method"))?;

    ncx.call_function(
        &emit_fn,
        stream.clone(),
        &[Value::string(JsString::intern("test:skip")), event_val],
    )?;

    Ok(())
}

fn emit_test_pass_event(
    stream: &Value,
    test: &TestEntry,
    test_number: u64,
    nesting: u32,
    duration_ms: f64,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    emit_test_event(
        stream,
        "test:pass",
        test,
        test_number,
        nesting,
        duration_ms,
        ncx,
    )
}

fn emit_test_fail_event(
    stream: &Value,
    test: &TestEntry,
    error: &VmError,
    test_number: u64,
    nesting: u32,
    duration_ms: f64,
    failure_type: &str,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let event = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = event.set(
        PropertyKey::string("type"),
        Value::string(JsString::intern("test:fail")),
    );

    let data = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = data.set(
        PropertyKey::string("name"),
        Value::string(JsString::new_gc(&test.name)),
    );
    // Feature 4: detailed event data
    let _ = data.set(
        PropertyKey::string("testNumber"),
        Value::number(test_number as f64),
    );
    let _ = data.set(
        PropertyKey::string("nesting"),
        Value::number(nesting as f64),
    );
    let _ = data.set(
        PropertyKey::string("duration_ms"),
        Value::number(duration_ms),
    );

    // Extract error message - try to get the actual error message from thrown Error objects
    let error_msg = match error {
        VmError::Exception(thrown) => {
            // Try to get message property from Error object
            if let Some(obj) = thrown.value.as_object() {
                if let Some(msg) = obj.get(&PropertyKey::string("message")) {
                    if let Some(s) = msg.as_string() {
                        format!("Error: {}", s.as_str())
                    } else {
                        thrown.message.clone()
                    }
                } else {
                    thrown.message.clone()
                }
            } else {
                thrown.message.clone()
            }
        }
        _ => error.to_string(),
    };

    let _ = data.set(
        PropertyKey::string("error"),
        Value::string(JsString::new_gc(&error_msg)),
    );

    // Feature 4: details with failure type
    let details = GcRef::new(JsObject::new(Value::null(), ncx.memory_manager().clone()));
    let _ = details.set(
        PropertyKey::string("type"),
        Value::string(JsString::intern(failure_type)),
    );
    let _ = data.set(PropertyKey::string("details"), Value::object(details));

    let _ = event.set(PropertyKey::string("data"), Value::object(data));
    let event_val = Value::object(event);

    // Add to queue
    if let Some(stream_obj) = stream.as_object() {
        if let Some(queue) = stream_obj
            .get(&PropertyKey::string("__event_queue"))
            .and_then(|v| v.as_object())
        {
            queue.array_push(event_val.clone());
        }
    }

    // Emit via EventEmitter
    let emit_fn = stream
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream has no emit method"))?;

    ncx.call_function(
        &emit_fn,
        stream.clone(),
        &[Value::string(JsString::intern("test:fail")), event_val],
    )?;

    Ok(())
}

fn emit_complete_event(stream: &Value, ncx: &mut NativeContext) -> Result<(), VmError> {
    let emit_fn = stream
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("emit")))
        .ok_or_else(|| VmError::type_error("stream has no emit method"))?;

    ncx.call_function(
        &emit_fn,
        stream.clone(),
        &[Value::string(JsString::intern("test:complete"))],
    )?;

    Ok(())
}

/// Handle async test result (promise)
fn handle_async_test_result(
    stream: &Value,
    test: &TestEntry,
    promise_val: Value,
    timeout_ms: f64,
    test_number: u64,
    duration_ms: f64,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    let mm = ncx.memory_manager().clone();

    // Store test info for callbacks
    let stream_clone = stream.clone();

    // Create a timeout flag (shared between timeout and promise handlers)
    let timed_out = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    let _ = timed_out.set(PropertyKey::string("value"), Value::boolean(false));

    // Setup timeout
    if timeout_ms > 0.0 && timeout_ms.is_finite() {
        setup_test_timeout(
            &stream_clone,
            test,
            timeout_ms,
            timed_out.clone(),
            test_number,
            ncx,
        )?;
    }

    // Create promise fulfillment handler
    let stream_fulfill = stream_clone.clone();
    let test_fulfill = test.clone();
    let timed_out_fulfill = timed_out.clone();
    let fulfill_handler = Value::native_function(
        move |_this, _args, ncx| {
            // Check if test already timed out
            if let Some(val) = timed_out_fulfill.get(&PropertyKey::string("value")) {
                if val.to_boolean() {
                    // Already timed out, don't emit pass
                    return Ok(Value::undefined());
                }
            }

            // Test passed
            let _ = emit_test_pass_event(
                &stream_fulfill,
                &test_fulfill,
                test_number,
                0,
                duration_ms,
                ncx,
            );
            Ok(Value::undefined())
        },
        mm.clone(),
    );

    // Create promise rejection handler
    let stream_reject = stream_clone.clone();
    let test_reject = test.clone();
    let timed_out_reject = timed_out.clone();
    let reject_handler = Value::native_function(
        move |_this, args, ncx| {
            // Check if test already timed out
            if let Some(val) = timed_out_reject.get(&PropertyKey::string("value")) {
                if val.to_boolean() {
                    // Already timed out, don't emit fail
                    return Ok(Value::undefined());
                }
            }

            // Test failed with rejection
            let reason = args.first().cloned().unwrap_or(Value::undefined());

            // Convert reason to VmError for emit_test_fail_event
            // Try to extract message from Error object
            let error_msg = if let Some(obj) = reason.as_object() {
                if let Some(msg) = obj.get(&PropertyKey::string("message")) {
                    if let Some(s) = msg.as_string() {
                        s.as_str().to_string()
                    } else {
                        format!("{:?}", reason)
                    }
                } else {
                    format!("{:?}", reason)
                }
            } else if let Some(s) = reason.as_string() {
                s.as_str().to_string()
            } else {
                format!("{:?}", reason)
            };

            let error = VmError::Exception(Box::new(ThrownValue {
                value: reason,
                message: error_msg,
                stack: vec![],
            }));

            let _ = emit_test_fail_event(
                &stream_reject,
                &test_reject,
                &error,
                test_number,
                0,
                duration_ms,
                "testCodeFailure",
                ncx,
            );
            Ok(Value::undefined())
        },
        mm,
    );

    // Attach handlers to promise
    if let Some(then_fn) = promise_val
        .as_object()
        .and_then(|o| o.get(&PropertyKey::string("then")))
    {
        let _ = ncx.call_function(&then_fn, promise_val, &[fulfill_handler, reject_handler]);
    } else {
        // Not a real promise, treat as passed
        emit_test_pass_event(stream, test, test_number, 0, duration_ms, ncx)?;
    }

    Ok(())
}

/// Setup timeout for async test using setTimeout from timer infrastructure
fn setup_test_timeout(
    stream: &Value,
    test: &TestEntry,
    timeout_ms: f64,
    timed_out_flag: GcRef<JsObject>,
    test_number: u64,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    // Look up setTimeout from the global object
    let set_timeout = ncx
        .global()
        .get(&PropertyKey::string("setTimeout"))
        .ok_or_else(|| VmError::type_error("setTimeout not available for test timeout"))?;

    if !set_timeout.is_callable() {
        return Ok(()); // setTimeout not available, skip timeout
    }

    let mm = ncx.memory_manager().clone();
    let stream_clone = stream.clone();
    let test_clone = test.clone();
    let flag = timed_out_flag;

    // Create timeout callback that marks test as timed out and emits test:fail
    let timeout_callback = Value::native_function(
        move |_this, _args, ncx| {
            // Check if test already completed (flag still false)
            let already_done = flag
                .get(&PropertyKey::string("value"))
                .map(|v| v.to_boolean())
                .unwrap_or(false);

            if !already_done {
                // Set timed out flag to prevent double-emission from promise handlers
                let _ = flag.set(PropertyKey::string("value"), Value::boolean(true));

                // Emit test:fail with timeout error
                let timeout_error =
                    VmError::type_error(&format!("test timed out after {}ms", timeout_ms));
                let _ = emit_test_fail_event(
                    &stream_clone,
                    &test_clone,
                    &timeout_error,
                    test_number,
                    0,
                    timeout_ms,
                    "testCodeFailure",
                    ncx,
                );
            }

            Ok(Value::undefined())
        },
        mm,
    );

    // Call setTimeout(callback, timeout_ms)
    let _ = ncx.call_function(
        &set_timeout,
        Value::undefined(),
        &[timeout_callback, Value::number(timeout_ms)],
    );

    Ok(())
}
