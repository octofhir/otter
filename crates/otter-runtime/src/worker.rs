//! Worker thread implementation for JavaScript execution
//!
//! Each worker maintains its own JSC context and processes jobs from a shared queue.

use crate::apis::register_all_apis;
use crate::context::JscContext;
use crate::error::{JscError, JscResult};
use crate::extension::Extension;
use crate::transpiler::transpile_typescript;
use crossbeam_channel::Receiver;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;
use tracing::{debug, error, warn};

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
pub(crate) fn run_worker(
    job_rx: Receiver<Job>,
    extensions: Vec<Extension>,
    shutdown: Arc<AtomicBool>,
) {
    let thread_name = std::thread::current()
        .name()
        .unwrap_or("otter-worker")
        .to_string();

    debug!(worker = %thread_name, "Worker starting");

    // Create JSC context for this worker
    let context = match JscContext::new() {
        Ok(ctx) => ctx,
        Err(e) => {
            error!(worker = %thread_name, error = %e, "Failed to create JSC context");
            return;
        }
    };

    // Register default APIs
    if let Err(e) = register_all_apis(context.raw()) {
        error!(worker = %thread_name, error = %e, "Failed to register APIs");
        return;
    }

    // Register extensions
    for ext in extensions {
        if let Err(e) = context.register_extension(ext) {
            error!(worker = %thread_name, error = %e, "Failed to register extension");
            return;
        }
    }

    debug!(worker = %thread_name, "Worker initialized");

    // Process jobs until shutdown
    loop {
        // Check shutdown flag
        if shutdown.load(Ordering::SeqCst) {
            debug!(worker = %thread_name, "Worker shutdown flag set");
            break;
        }

        // Poll event loop to process pending async ops
        if let Err(e) = context.poll_event_loop() {
            warn!(worker = %thread_name, error = %e, "Event loop poll failed");
        }

        // Try to receive a job with timeout to allow event loop polling
        match job_rx.recv_timeout(Duration::from_millis(10)) {
            Ok(job) => {
                match job {
                    Job::Shutdown => {
                        debug!(worker = %thread_name, "Received shutdown signal");
                        break;
                    }
                    Job::Eval {
                        script,
                        source_url,
                        response,
                    } => {
                        let result = execute_eval(&context, &script, source_url.as_deref());
                        let _ = response.send(result);
                    }
                    Job::EvalTypeScript {
                        code,
                        source_url,
                        response,
                    } => {
                        let result =
                            execute_typescript(&context, &code, source_url.as_deref());
                        let _ = response.send(result);
                    }
                    Job::Call {
                        function,
                        args,
                        response,
                    } => {
                        let result = execute_call(&context, &function, args);
                        let _ = response.send(result);
                    }
                }
            }
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                // Continue polling event loop
                continue;
            }
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                debug!(worker = %thread_name, "Job channel disconnected");
                break;
            }
        }
    }

    // Drain remaining tasks before exiting
    drain_event_loop(&context);

    debug!(worker = %thread_name, "Worker stopped");
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

    // Convert result to JSON
    result_to_json(context, result)
}

/// Transpile and execute TypeScript code
fn execute_typescript(
    context: &JscContext,
    code: &str,
    source_url: Option<&str>,
) -> JscResult<serde_json::Value> {
    // Transpile TypeScript to JavaScript
    let js_code = transpile_typescript(code).map_err(|e| JscError::script_error("SyntaxError", e.to_string()))?;

    execute_eval(context, &js_code, source_url)
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
    context: &JscContext,
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
