//! Worker thread implementation for JavaScript execution
//!
//! Each worker maintains its own JSC context and processes jobs from a shared queue.
//! Workers handle panics gracefully and report errors via the response channel.

use crate::apis::register_all_apis;
use crate::context::JscContext;
use crate::engine::EngineStats;
use crate::error::{JscError, JscResult};
use crate::extension::Extension;
use crate::transpiler::transpile_typescript;
use crossbeam_channel::Receiver;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, info_span, warn};

/// Job submitted to the engine for execution
pub(crate) enum Job {
    /// Evaluate JavaScript code
    Eval {
        script: String,
        source_url: Option<String>,
        response: oneshot::Sender<JscResult<serde_json::Value>>,
    },
    /// Evaluate TypeScript code (transpile + execute)
    EvalTypeScript {
        code: String,
        source_url: Option<String>,
        response: oneshot::Sender<JscResult<serde_json::Value>>,
    },
    /// Call a global function
    Call {
        function: String,
        args: Vec<serde_json::Value>,
        response: oneshot::Sender<JscResult<serde_json::Value>>,
    },
    /// Shutdown signal
    Shutdown,
}

/// Run a worker thread that processes jobs from the queue
///
/// This function creates a JSC context on the current thread and processes
/// jobs until shutdown is signaled. Panics during job execution are caught
/// and converted to errors.
///
/// # Arguments
///
/// * `job_rx` - Channel receiver for incoming jobs
/// * `extensions` - Extensions to register in the worker context
/// * `shutdown` - Shared flag to signal worker shutdown
/// * `stats` - Shared statistics counter
/// * `tokio_handle` - Tokio runtime handle for async operations (required)
pub(crate) fn run_worker(
    job_rx: Receiver<Job>,
    extensions: Vec<Extension>,
    shutdown: Arc<AtomicBool>,
    stats: Arc<EngineStats>,
    tokio_handle: &tokio::runtime::Handle,
) {
    let thread_name = std::thread::current()
        .name()
        .unwrap_or("otter-worker")
        .to_string();

    let _span = info_span!("worker", name = %thread_name).entered();
    debug!("Worker starting");

    // Store Tokio handle in thread-local for async operations
    crate::extension::set_tokio_handle(tokio_handle.clone());

    // Create JSC context for this worker
    let context = match JscContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            error!(error = %e, "Failed to create JSC context");
            return;
        }
    };

    // Register default APIs
    if let Err(e) = register_all_apis(context.raw()) {
        error!(error = %e, "Failed to register APIs");
        return;
    }

    // Register extensions
    for ext in extensions {
        if let Err(e) = context.register_extension(ext) {
            error!(error = %e, "Failed to register extension");
            return;
        }
    }

    debug!("Worker initialized");

    // Process jobs until shutdown
    loop {
        // Check shutdown flag
        if shutdown.load(Ordering::SeqCst) {
            debug!("Worker shutdown flag set");
            break;
        }

        // Poll event loop to process pending async ops
        if let Err(e) = context.poll_event_loop() {
            warn!(error = %e, "Event loop poll failed");
        }

        // Try to receive a job with timeout to allow event loop polling
        match job_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(job) => {
                execute_job(&context, job, &stats);
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Continue polling event loop
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                debug!("Job channel disconnected");
                break;
            }
        }
    }

    // Drain remaining tasks before exiting
    drain_event_loop(&context);

    debug!("Worker stopped");
}

/// Execute a single job with panic handling
fn execute_job(context: &JscContext, job: Job, stats: &EngineStats) {
    match job {
        Job::Shutdown => {
            debug!("Received shutdown signal");
            // Signal handled by caller
        }
        Job::Eval {
            script,
            source_url,
            response,
        } => {
            let _span =
                info_span!("eval", source = source_url.as_deref().unwrap_or("<eval>")).entered();
            let result = execute_with_panic_handler(|| {
                execute_eval(context, &script, source_url.as_deref())
            });
            update_stats(stats, &result);
            let _ = response.send(result);
        }
        Job::EvalTypeScript {
            code,
            source_url,
            response,
        } => {
            let _span = info_span!(
                "eval_ts",
                source = source_url.as_deref().unwrap_or("<eval>")
            )
            .entered();
            let result = execute_with_panic_handler(|| {
                execute_typescript(context, &code, source_url.as_deref())
            });
            update_stats(stats, &result);
            let _ = response.send(result);
        }
        Job::Call {
            function,
            args,
            response,
        } => {
            let _span = info_span!("call", function = %function).entered();
            let result = execute_with_panic_handler(|| execute_call(context, &function, args));
            update_stats(stats, &result);
            let _ = response.send(result);
        }
    }
}

/// Execute a closure with panic handling
fn execute_with_panic_handler<F>(f: F) -> JscResult<serde_json::Value>
where
    F: FnOnce() -> JscResult<serde_json::Value>,
{
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(result) => result,
        Err(panic) => {
            let message = if let Some(s) = panic.downcast_ref::<&str>() {
                format!("Worker panic: {}", s)
            } else if let Some(s) = panic.downcast_ref::<String>() {
                format!("Worker panic: {}", s)
            } else {
                "Worker panic: unknown error".to_string()
            };
            error!("{}", message);
            Err(JscError::internal(message))
        }
    }
}

/// Update engine statistics based on job result
fn update_stats(stats: &EngineStats, result: &JscResult<serde_json::Value>) {
    stats.jobs_completed.fetch_add(1, Ordering::Relaxed);
    if result.is_err() {
        stats.jobs_failed.fetch_add(1, Ordering::Relaxed);
    }
}

/// Execute JavaScript code and return result as JSON
fn execute_eval(
    context: &JscContext,
    script: &str,
    source_url: Option<&str>,
) -> JscResult<serde_json::Value> {
    let result = if let Some(url) = source_url {
        context.eval_with_source(script, url)?
    } else {
        context.eval(script)?
    };

    // Run event loop to handle any pending promises
    run_event_loop_briefly(context)?;

    // Check if result is a Promise and unwrap it
    if result.is_promise() {
        return unwrap_promise(context, result);
    }

    // Convert result to JSON
    result_to_json(context, result)
}

/// Unwrap a Promise by waiting for it to resolve
fn unwrap_promise(
    context: &JscContext,
    promise: crate::value::JscValue,
) -> JscResult<serde_json::Value> {
    // We'll use global variables to track resolution state
    // This approach works reliably with JSC's event loop

    // Generate unique variable names to avoid collisions
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    static PROMISE_COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = PROMISE_COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
    let resolved_var = format!("__promise_resolved_{}", id);
    let value_var = format!("__promise_value_{}", id);
    let error_var = format!("__promise_error_{}", id);
    let promise_var = format!("__promise_{}", id);

    // Initialize tracking variables
    context.set_global(&resolved_var, &context.boolean(false))?;
    context.set_global(&value_var, &context.null())?;
    context.set_global(&error_var, &context.null())?;
    context.set_global(&promise_var, &promise)?;

    // Attach .then() and .catch() handlers to the promise
    let handler_code = format!(
        r#"
        {promise_var}.then(function(v) {{
            {value_var} = v;
            {resolved_var} = true;
        }}).catch(function(e) {{
            {error_var} = e && e.message ? e.message : String(e);
            {resolved_var} = true;
        }});
        "#,
        promise_var = promise_var,
        value_var = value_var,
        resolved_var = resolved_var,
        error_var = error_var,
    );

    context.eval(&handler_code)?;

    // Poll event loop until the promise resolves or timeout
    let timeout = Duration::from_secs(30);
    let start = std::time::Instant::now();

    loop {
        // Poll the event loop to process microtasks
        context.poll_event_loop()?;

        // Check if resolved
        let resolved = context.get_global(&resolved_var)?;
        if resolved.to_bool() {
            break;
        }

        // Check timeout
        if start.elapsed() >= timeout {
            cleanup_promise_vars(context, &resolved_var, &value_var, &error_var, &promise_var);
            return Err(JscError::Timeout(timeout.as_millis() as u64));
        }

        // Small sleep to avoid busy loop
        std::thread::sleep(Duration::from_millis(1));
    }

    // Check for rejection error
    let error = context.get_global(&error_var)?;
    if !error.is_null() {
        let error_msg = error
            .to_string()
            .unwrap_or_else(|_| "Promise rejected".to_string());
        cleanup_promise_vars(context, &resolved_var, &value_var, &error_var, &promise_var);
        return Err(JscError::script_error("Error", error_msg));
    }

    // Get the resolved value
    let value = context.get_global(&value_var)?;
    cleanup_promise_vars(context, &resolved_var, &value_var, &error_var, &promise_var);

    // Convert to JSON
    result_to_json(context, value)
}

/// Clean up temporary global variables used for promise unwrapping
fn cleanup_promise_vars(
    context: &JscContext,
    resolved_var: &str,
    value_var: &str,
    error_var: &str,
    promise_var: &str,
) {
    let _ = context.eval(&format!(
        "delete {}; delete {}; delete {}; delete {};",
        resolved_var, value_var, error_var, promise_var
    ));
}

/// Transpile and execute TypeScript code
fn execute_typescript(
    context: &JscContext,
    code: &str,
    source_url: Option<&str>,
) -> JscResult<serde_json::Value> {
    // Transpile TypeScript to JavaScript
    let result = transpile_typescript(code)
        .map_err(|e| JscError::script_error("SyntaxError", e.to_string()))?;

    execute_eval(context, &result.code, source_url)
}

/// Call a global function with arguments
fn execute_call(
    context: &JscContext,
    function: &str,
    args: Vec<serde_json::Value>,
) -> JscResult<serde_json::Value> {
    // Build the call expression
    let args_json: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    let script = format!("{}({})", function, args_json.join(", "));

    execute_eval(context, &script, None)
}

/// Convert a JscValue result to serde_json::Value
fn result_to_json(
    _context: &JscContext,
    value: crate::value::JscValue,
) -> JscResult<serde_json::Value> {
    // Handle undefined/null
    if value.is_undefined() || value.is_null() {
        return Ok(serde_json::Value::Null);
    }

    // Try to get JSON representation
    let json_str = value.to_json()?;
    serde_json::from_str(&json_str).map_err(Into::into)
}

/// Run event loop briefly to handle pending async operations
fn run_event_loop_briefly(context: &JscContext) -> JscResult<()> {
    let timeout = Duration::from_millis(100);
    let start = std::time::Instant::now();

    while context.has_pending_tasks() && start.elapsed() < timeout {
        context.poll_event_loop()?;
        std::thread::sleep(Duration::from_millis(1));
    }

    Ok(())
}

/// Drain the event loop before worker shutdown
fn drain_event_loop(context: &JscContext) {
    let timeout = Duration::from_millis(500);
    let start = std::time::Instant::now();

    while context.has_pending_tasks() && start.elapsed() < timeout {
        if let Err(e) = context.poll_event_loop() {
            warn!(error = %e, "Event loop drain failed");
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
