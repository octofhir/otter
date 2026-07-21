//! `$262.agent.*` host harness (slice 19c).
//!
//! Test262 cross-agent tests require a host-defined agent model
//! (start agents, broadcast a `SharedArrayBuffer`, wake up
//! `Atomics.wait` waiters from another isolate, etc.). This module
//! implements that surface inside the runner through small OS-thread
//! agent runtimes instead of exposing product Worker globals to
//! conformance tests.
//!
//! Architecture:
//!
//! - One [`AgentRegistry`] lives in a process-wide `LazyLock`. It
//!   owns:
//!   * one `mpsc::Sender<BroadcastMessage>` plus one thread join
//!     handle per started agent so the parent thread's
//!     `$262.agent.broadcast` can fan out to every running agent;
//!   * a FIFO `VecDeque<String>` for `$262.agent.report` /
//!     `$262.agent.getReport`.
//! - Agent inboxes are keyed by [`thread::ThreadId`] in
//!   [`AGENT_INBOXES`] so `receiveBroadcast` blocks on the right
//!   channel without thread-local state.
//! - The shared buffer rides through the channel as an
//!   `Arc<SharedBody>`. The receiving agent rewraps it via
//!   [`otter_vm::NativeScope::shared_array_buffer`] before handing
//!   it to the user handler, so both sides observe the same backing
//!   storage and the same `Atomics.wait` registry id.
//!
//! Hardening:
//!
//! - The parent thread captures `Vec<Sender>` then releases the
//!   registry lock **before** sending — agents may block on recv,
//!   and holding the registry lock across a blocking send would
//!   serialise broadcast dispatch.
//! - The parent never registers an inbox, so it never receives its
//!   own broadcast messages.
//! - Each agent is a fresh engine-shell runtime with a private heap;
//!   the only state shared with the parent is the SAB's
//!   `Arc<SharedBody>`.
//!
//! # See also
//!
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md#host-defined-functions>
//! - `docs/workers-262-plan.md` — slice 19c plan.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use otter_runtime::{
    InterruptHandle, OtterError, Runtime, RuntimeGlobalInstaller, RuntimeRealmContext, SourceInput,
};
use otter_vm::binary::array_buffer::SharedBody;
use otter_vm::string::JsString;
use otter_vm::{NativeCtx, NativeError, NativeFastFn, Value};

use crate::harness::D262_HOST_PREAMBLE;

/// One broadcast message handed from the parent thread to every
/// running agent through its [`AGENT_INBOXES`] channel.
#[derive(Clone)]
struct BroadcastMessage {
    /// Shared backing for the cross-thread `SharedArrayBuffer`.
    sab: Arc<SharedBody>,
    /// Optional companion number per `$262.agent.broadcast(sab, n)`.
    num: Option<f64>,
}

/// Per-process agent registry.
struct AgentRegistry {
    /// One sender per running agent.
    senders: Vec<mpsc::Sender<BroadcastMessage>>,
    /// Join handles for started agents. Reset joins previous-test agents
    /// after closing their broadcast channels so their private heaps return
    /// pages to the process-global GC cage before the next test starts.
    handles: Vec<thread::JoinHandle<()>>,
    /// Cooperative interrupt handles for agent runtimes. Reset trips these
    /// before joining so agents spinning in JS code can leave their VM loops.
    interrupts: Vec<InterruptHandle>,
    /// `$262.agent.report` / `$262.agent.getReport` FIFO.
    reports: VecDeque<String>,
}

static AGENTS: LazyLock<Mutex<AgentRegistry>> = LazyLock::new(|| {
    Mutex::new(AgentRegistry {
        senders: Vec::new(),
        handles: Vec::new(),
        interrupts: Vec::new(),
        reports: VecDeque::new(),
    })
});

/// Receiver end for each live agent thread. The parent thread never
/// inserts an inbox, so a stray `receiveBroadcast` call outside an
/// agent fails deterministically with `TypeError`.
static AGENT_INBOXES: LazyLock<Mutex<HashMap<thread::ThreadId, mpsc::Receiver<BroadcastMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const RECEIVE_BROADCAST_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Monotonic clock anchor — `monotonicNow()` returns
/// milliseconds since the first call inside the process.
static MONOTONIC_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Reset the cross-agent shared state. Called from the per-test
/// driver between tests so a previous test's leftover agents /
/// reports do not bleed into the next.
pub fn reset_for_next_test() {
    let (handles, interrupts) = {
        let mut reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.clear();
        reg.reports.clear();
        (
            std::mem::take(&mut reg.handles),
            std::mem::take(&mut reg.interrupts),
        )
    };
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .clear();
    for interrupt in interrupts {
        interrupt.interrupt();
    }
    otter_vm::atomics_wait::cancel_all_waiters();
    for handle in handles {
        let _ = handle.join();
    }
}

/// Install every `__otter_agent_*` native global on the given
/// runtime. The runner calls this once per fresh runtime before
/// the harness preamble runs so the JS-side `$262.agent` object
/// in [`D262_HOST_PREAMBLE`] resolves to live bindings.
pub fn install_natives(runtime: &mut RuntimeRealmContext<'_>) -> Result<(), OtterError> {
    for (name, length, call) in NATIVES {
        runtime.install_native_global(name, *length, *call)?;
    }
    Ok(())
}

/// `(name, length, fn)` table installed by [`install_natives`].
const NATIVES: &[(&str, u8, NativeFastFn)] = &[
    ("__otter_is_htmldda", 0, is_html_dda),
    ("__otter_eval_script", 1, eval_script),
    ("__otter_create_realm", 0, create_realm),
    ("__otter_agent_start", 1, agent_start),
    ("__otter_agent_broadcast", 2, agent_broadcast),
    ("__otter_agent_get_report", 0, agent_get_report),
    ("__otter_agent_sleep", 1, agent_sleep),
    ("__otter_agent_monotonic_now", 0, agent_monotonic_now),
    (
        "__otter_agent_receive_broadcast",
        1,
        agent_receive_broadcast,
    ),
    ("__otter_agent_report", 1, agent_report),
    ("__otter_agent_leaving", 0, agent_leaving),
];

// =====================================================================
// Helpers
// =====================================================================

fn type_err(reason: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name: "$262.agent",
        reason: reason.into(),
    }
}

fn is_html_dda(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::null())
}

fn map_eval_script_result(
    ctx: &mut NativeCtx<'_>,
    result: Result<Value, otter_vm::VmError>,
    name: &'static str,
) -> Result<Value, NativeError> {
    let detail = ctx.interp_mut().take_error_detail();
    let detail_msg = match &detail {
        Some(otter_vm::ErrorDetail::Message(m))
        | Some(otter_vm::ErrorDetail::Name(m))
        | Some(otter_vm::ErrorDetail::Uncaught(m)) => m.to_string(),
        _ => String::new(),
    };
    result.map_err(|err| match err {
        otter_vm::VmError::SyntaxError => NativeError::SyntaxError {
            name,
            reason: detail_msg,
        },
        // An uncaught throw from the script body arrives rendered
        // ("SyntaxError: …"); recover the spec error class so
        // `assert.throws(SyntaxError, …)` sees the right
        // constructor.
        otter_vm::VmError::Uncaught => {
            let render = detail_msg;
            let class_mapped = [
                ("SyntaxError", 0u8),
                ("TypeError", 1),
                ("ReferenceError", 2),
                ("RangeError", 3),
            ]
            .iter()
            .find(|(prefix, _)| render.starts_with(prefix))
            .map(|(_, kind)| *kind);
            let reason = render
                .split_once(": ")
                .map(|(_, tail)| tail.to_string())
                .unwrap_or(render.clone());
            match class_mapped {
                Some(0) => NativeError::SyntaxError { name, reason },
                Some(2) => NativeError::ReferenceError { name, reason },
                Some(3) => NativeError::RangeError { name, reason },
                _ => NativeError::TypeError {
                    name,
                    reason: render,
                },
            }
        }
        err => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    })
}

/// `$262.evalScript(source)` — INTERPRETING.md host API: parse
/// `source` as an ECMAScript Script and run it in the current realm
/// with §16.1.7 GlobalDeclarationInstantiation semantics.
fn eval_script(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let result = ctx.interp_mut().run_host_script(&arg);
    map_eval_script_result(ctx, result, "evalScript")
}

fn realm_eval_script(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    run_realm_script_capture(ctx, args, captures, "evalScript")
}

fn realm_global_eval(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
) -> Result<Value, NativeError> {
    run_realm_script_capture(ctx, args, captures, "eval")
}

fn run_realm_script_capture(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    captures: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let Some(global) = captures.first().and_then(|value| value.as_object()) else {
        return Err(type_err("realm eval lost its global"));
    };
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let result = ctx
        .interp_mut()
        .run_host_script_in_realm_global(global, &arg);
    map_eval_script_result(ctx, result, name)
}

fn create_realm(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let global =
        ctx.interp_mut()
            .create_host_realm_global()
            .map_err(|err| NativeError::TypeError {
                name: "$262.createRealm",
                reason: err.to_string(),
            })?;
    // Captured dynamic native closures remain an explicit raw host boundary for
    // this checkpoint. Persist the capture and both generated functions while
    // the next closure allocation runs, then switch back to `NativeScope` for
    // all JS-visible object construction.
    let mut persistent = Vec::with_capacity(3);
    let result = (|| {
        let global_root = ctx.persistent_root_insert(Value::object(global));
        persistent.push(global_root);
        let global_value = ctx
            .persistent_root_get(global_root)
            .ok_or_else(|| type_err("realm global root disappeared"))?;
        let eval_value = ctx
            .native_value(
                "$262.createRealm.evalScript",
                smallvec::smallvec![global_value],
                realm_eval_script,
            )
            .map_err(|_| NativeError::TypeError {
                name: "$262.createRealm",
                reason: "evalScript allocation failed".to_string(),
            })?;
        let eval_root = ctx.persistent_root_insert(eval_value);
        persistent.push(eval_root);

        let global_value = ctx
            .persistent_root_get(global_root)
            .ok_or_else(|| type_err("realm global root disappeared"))?;
        let global_eval_value = ctx
            .native_value(
                "$262.createRealm.global.eval",
                smallvec::smallvec![global_value],
                realm_global_eval,
            )
            .map_err(|_| NativeError::TypeError {
                name: "$262.createRealm",
                reason: "global eval allocation failed".to_string(),
            })?;
        let global_eval_root = ctx.persistent_root_insert(global_eval_value);
        persistent.push(global_eval_root);

        let global = ctx
            .persistent_root_get(global_root)
            .ok_or_else(|| type_err("realm global root disappeared"))?;
        let eval_value = ctx
            .persistent_root_get(eval_root)
            .ok_or_else(|| type_err("realm eval root disappeared"))?;
        let global_eval_value = ctx
            .persistent_root_get(global_eval_root)
            .ok_or_else(|| type_err("realm global eval root disappeared"))?;
        ctx.scope(|mut scope| {
            let global = scope.value(global);
            let eval_value = scope.value(eval_value);
            let global_eval_value = scope.value(global_eval_value);
            let realm = scope.object()?;
            scope.define(
                realm,
                "global",
                global,
                otter_vm::object::PropertyFlags::new(true, true, true),
            )?;
            scope.define(
                realm,
                "evalScript",
                eval_value,
                otter_vm::object::PropertyFlags::new(true, true, true),
            )?;
            scope.define(
                global,
                "eval",
                global_eval_value,
                otter_vm::object::PropertyFlags::new(true, false, true),
            )?;
            Ok(scope.finish(realm))
        })
    })();
    for root in persistent {
        let _ = ctx.persistent_root_remove(root);
    }
    result
}

fn arg_to_string(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<String, NativeError> {
    if let Some(s) = value.as_string(ctx.heap()) {
        return Ok(s.to_lossy_string(ctx.heap()));
    }
    if value.is_undefined() {
        return Ok("undefined".to_string());
    }
    if value.is_null() {
        return Ok("null".to_string());
    }
    if let Some(b) = value.as_boolean() {
        return Ok(if b { "true" } else { "false" }.to_string());
    }
    if let Some(n) = value.as_number() {
        return Ok(format!("{}", n.as_f64()));
    }
    if let Some(b) = value.as_big_int() {
        return Ok(b.to_decimal_string(ctx.heap()));
    }
    Err(type_err("expected a string argument"))
}

// =====================================================================
// $262.agent.start(source)
// =====================================================================

fn agent_start(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let source = arg_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
    let (tx, rx) = mpsc::channel::<BroadcastMessage>();

    let (interrupt, handle) = spawn_thread_agent(source, rx)?;
    {
        let mut reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.push(tx);
        reg.interrupts.push(interrupt);
        reg.handles.push(handle);
    }

    Ok(Value::undefined())
}

fn spawn_thread_agent(
    source: String,
    inbox: mpsc::Receiver<BroadcastMessage>,
) -> Result<(InterruptHandle, thread::JoinHandle<()>), NativeError> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<InterruptHandle, String>>();
    let handle = thread::Builder::new()
        .name("test262-agent".to_string())
        .spawn(move || run_agent_thread(source, inbox, ready_tx))
        .map_err(|err| type_err(format!("agent thread spawn failed: {err}")))?;

    match ready_rx.recv() {
        Ok(Ok(interrupt)) => Ok((interrupt, handle)),
        Ok(Err(reason)) => {
            let _ = handle.join();
            Err(type_err(reason))
        }
        Err(err) => {
            let _ = handle.join();
            Err(type_err(format!("agent thread setup failed: {err}")))
        }
    }
}

fn run_agent_thread(
    source: String,
    inbox: mpsc::Receiver<BroadcastMessage>,
    ready: mpsc::Sender<Result<InterruptHandle, String>>,
) {
    let thread_id = thread::current().id();
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .insert(thread_id, inbox);
    let mut runtime = match Runtime::builder()
        .timeout(Duration::ZERO)
        .max_heap_bytes(0)
        .allow_blocking_atomics_wait(true)
        .process_global(false)
        .worker_global(false)
        .global_installer(RuntimeGlobalInstaller::new(install_natives))
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            AGENT_INBOXES
                .lock()
                .expect("agent inbox registry poisoned")
                .remove(&thread_id);
            let _ = ready.send(Err(err.to_string()));
            return;
        }
    };
    let interrupt = runtime.interrupt_handle();
    if ready.send(Ok(interrupt)).is_err() {
        AGENT_INBOXES
            .lock()
            .expect("agent inbox registry poisoned")
            .remove(&thread_id);
        return;
    }

    let mut combined = String::with_capacity(D262_HOST_PREAMBLE.len() + source.len() + 1);
    combined.push_str(D262_HOST_PREAMBLE);
    combined.push('\n');
    combined.push_str(&source);

    match runtime.run_script(SourceInput::from_javascript(combined), "<test262-agent>") {
        Ok(_) | Err(OtterError::Interrupted) => {}
        Err(err) => {
            AGENTS
                .lock()
                .expect("agent registry poisoned")
                .reports
                .push_back(format!("agent error: {err}"));
        }
    }
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .remove(&thread_id);
}

// =====================================================================
// $262.agent.broadcast(sab, num?)
// =====================================================================

fn agent_broadcast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let sab_value = args.first().copied().unwrap_or(Value::undefined());
    let Some(buf) = sab_value.as_array_buffer() else {
        return Err(type_err("first argument must be a SharedArrayBuffer"));
    };
    let Some(shared) = buf.as_shared_arc(ctx.heap()) else {
        return Err(type_err("first argument must be a SharedArrayBuffer"));
    };
    let num = match args.get(1) {
        None => None,
        Some(v) if v.is_undefined() => None,
        Some(v) => match v.as_number() {
            Some(n) => Some(n.as_f64()),
            None => return Err(type_err("second argument must be a Number or omitted")),
        },
    };
    let msg = BroadcastMessage { sab: shared, num };
    // Capture sender list under lock then drop the lock before
    // sending so a slow recv does not stall other broadcasts.
    let senders: Vec<mpsc::Sender<BroadcastMessage>> = {
        let reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.clone()
    };
    for tx in senders {
        let _ = tx.send(msg.clone());
    }
    Ok(Value::undefined())
}

// =====================================================================
// $262.agent.receiveBroadcast(handler)
// =====================================================================

fn agent_receive_broadcast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let handler = args.first().copied().unwrap_or(Value::undefined());
    if !handler.is_callable() {
        return Err(type_err("receiveBroadcast handler must be a function"));
    }

    // Take the receiver out of the registry while we block on recv.
    // `rx.recv()` takes `&self` so the receiver remains valid for
    // the next call; we put it back after recv returns.
    let thread_id = thread::current().id();
    let rx = AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .remove(&thread_id);
    let Some(rx) = rx else {
        return Err(type_err("receiveBroadcast called outside an agent thread"));
    };
    let interrupt = ctx.interrupt_handle();
    let result = loop {
        if interrupt.is_set() {
            break Err(mpsc::RecvTimeoutError::Timeout);
        }
        match rx.recv_timeout(RECEIVE_BROADCAST_POLL_INTERVAL) {
            Ok(msg) => break Ok(msg),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                break Err(mpsc::RecvTimeoutError::Disconnected);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if interrupt.is_set() {
                    break Err(mpsc::RecvTimeoutError::Timeout);
                }
            }
        }
    };
    // Always put the receiver back. If it's closed, the next
    // recv() returns `Disconnected` and the caller handles it.
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .insert(thread_id, rx);
    let msg = match result {
        Ok(m) => m,
        Err(mpsc::RecvTimeoutError::Timeout) if interrupt.is_set() => {
            return Err(NativeError::Interrupted);
        }
        Err(_) => {
            // Senders all dropped before any broadcast arrived.
            return Ok(Value::undefined());
        }
    };

    if ctx.execution_context().is_none() {
        return Err(type_err("missing execution context"));
    }
    ctx.scope(|mut scope| {
        let handler = scope.value(handler);
        let sab = scope
            .shared_array_buffer(msg.sab)
            .map_err(|_| type_err("out of memory while wrapping SharedArrayBuffer"))?;
        let num = scope.value(match msg.num {
            Some(n) => Value::number_f64(n),
            None => Value::undefined(),
        });
        let this_value = scope.undefined();
        match scope.call(handler, this_value, &[sab, num]) {
            Ok(result) => Ok(scope.finish(result)),
            Err(NativeError::Thrown { message, .. }) => Err(NativeError::Thrown {
                name: "$262.agent.receiveBroadcast",
                message,
            }),
            Err(other) => Err(type_err(other.to_string())),
        }
    })
}

// =====================================================================
// $262.agent.sleep(ms) / monotonicNow()
// =====================================================================

fn agent_sleep(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let ms = match args.first() {
        None => 0.0,
        Some(v) if v.is_undefined() => 0.0,
        Some(v) => match v.as_number() {
            Some(n) => n.as_f64(),
            None => return Err(type_err("sleep argument must be a Number")),
        },
    };
    if ms.is_finite() && ms > 0.0 {
        thread::sleep(Duration::from_millis(ms as u64));
    }
    Ok(Value::undefined())
}

fn agent_monotonic_now(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let base = *MONOTONIC_BASE;
    let elapsed_ms = base.elapsed().as_secs_f64() * 1000.0;
    Ok(Value::number_f64(elapsed_ms))
}

// =====================================================================
// $262.agent.report(msg) / getReport()
// =====================================================================

fn agent_report(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let msg = arg_to_string(ctx, args.first().unwrap_or(&Value::undefined()))?;
    let mut reg = AGENTS.lock().expect("agent registry poisoned");
    reg.reports.push_back(msg);
    Ok(Value::undefined())
}

fn agent_get_report(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let mut reg = AGENTS.lock().expect("agent registry poisoned");
    let msg = reg.reports.pop_front();
    drop(reg);
    match msg {
        None => Ok(Value::null()),
        Some(s) => {
            let (interp, _exec) = ctx.interp_mut_and_context();
            let js = JsString::from_str(&s, interp.gc_heap_mut())
                .map_err(|e| type_err(format!("string alloc: {e}")))?;
            Ok(Value::string(js))
        }
    }
}

// =====================================================================
// $262.agent.leaving()
// =====================================================================

fn agent_leaving(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{LazyLock, Mutex};

    const TEST262_STA: &str = include_str!("../../../vendor/test262/harness/sta.js");
    const TEST262_ASSERT: &str = include_str!("../../../vendor/test262/harness/assert.js");
    const TEST262_ATOMICS_HELPER: &str =
        include_str!("../../../vendor/test262/harness/atomicsHelper.js");
    static AGENT_TEST_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[test]
    fn agent_start_reports_without_worker_global() {
        let _guard = AGENT_TEST_LOCK.lock().expect("agent test lock poisoned");
        reset_for_next_test();
        let mut runtime = Runtime::builder()
            .timeout(Duration::ZERO)
            .max_heap_bytes(64 * 1024 * 1024)
            .process_global(false)
            .worker_global(false)
            .global_installer(RuntimeGlobalInstaller::new(install_natives))
            .build()
            .expect("runtime");
        let source = format!(
            r#"{}
            $262.agent.start("$262.agent.report('ok');");
            var report = null;
            for (var i = 0; i < 100 && report === null; i++) {{
                $262.agent.sleep(1);
                report = $262.agent.getReport();
            }}
            report;
            "#,
            D262_HOST_PREAMBLE
        );
        let completion = runtime
            .run_script(SourceInput::from_javascript(source), "<agent-thread-test>")
            .expect("script")
            .completion_string()
            .to_string();
        reset_for_next_test();
        assert_eq!(completion, "ok");
    }

    #[test]
    fn agent_notify_reports_wait_outcomes() {
        let _guard = AGENT_TEST_LOCK.lock().expect("agent test lock poisoned");
        reset_for_next_test();
        let mut runtime = Runtime::builder()
            .timeout(Duration::ZERO)
            .max_heap_bytes(64 * 1024 * 1024)
            .allow_blocking_atomics_wait(true)
            .process_global(false)
            .worker_global(false)
            .global_installer(RuntimeGlobalInstaller::new(install_natives))
            .build()
            .expect("runtime");
        let source = format!(
            r#""use strict";
            {}
            $262.agent.waitUntil = function(typedArray, index, expected) {{
                var agents = 0;
                while ((agents = Atomics.load(typedArray, index)) !== expected) {{}}
            }};
            $262.agent.safeBroadcast = function(typedArray) {{
                $262.agent.broadcast(typedArray.buffer);
            }};
            $262.agent.tryYield = function() {{
                $262.agent.sleep(50);
            }};
            $262.agent.trySleep = function(ms) {{
                $262.agent.sleep(ms);
            }};
            for (var i = 0; i < 3; i++) {{
                $262.agent.start(`
                    $262.agent.receiveBroadcast(function(sab) {{
                        const i32a = new Int32Array(sab);
                        Atomics.add(i32a, 1, 1);
                        $262.agent.report(Atomics.wait(i32a, 0, 0, 200));
                        $262.agent.leaving();
                    }});
                `);
            }}
            const i32a = new Int32Array(new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * 4));
            $262.agent.safeBroadcast(i32a);
            $262.agent.waitUntil(i32a, 1, 3);
            $262.agent.tryYield();
            Atomics.notify(i32a, 0, 1);
            $262.agent.trySleep(250);
            const reports = [];
            for (var i = 0; i < 3; i++) {{
                reports.push($262.agent.getReport());
            }}
            reports.sort();
            reports.join(",");
            "#,
            D262_HOST_PREAMBLE
        );
        let completion = runtime
            .run_script(SourceInput::from_javascript(source), "<agent-notify-test>")
            .expect("script")
            .completion_string()
            .to_string();
        reset_for_next_test();
        assert_eq!(completion, "ok,timed-out,timed-out");
    }

    #[test]
    fn agent_notify_test262_helper_path_passes() {
        let _guard = AGENT_TEST_LOCK.lock().expect("agent test lock poisoned");
        reset_for_next_test();
        let mut runtime = Runtime::builder()
            .timeout(Duration::ZERO)
            .max_heap_bytes(64 * 1024 * 1024)
            .allow_blocking_atomics_wait(true)
            .process_global(false)
            .worker_global(false)
            .global_installer(RuntimeGlobalInstaller::new(install_natives))
            .build()
            .expect("runtime");
        let source = format!(
            r#""use strict";
            {}
            {}
            {}
            {}
            const WAIT_INDEX = 0;
            const RUNNING = 1;
            const NOTIFYCOUNT = 1;
            const NUMAGENT = 3;
            const BUFFER_SIZE = 4;
            const TIMEOUT = $262.agent.timeouts.long;

            for (var i = 0; i < NUMAGENT; i++ ) {{
              $262.agent.start(`
                $262.agent.receiveBroadcast(function(sab) {{
                  const i32a = new Int32Array(sab);
                  Atomics.add(i32a, ${{RUNNING}}, 1);
                  $262.agent.report(Atomics.wait(i32a, ${{WAIT_INDEX}}, 0, ${{TIMEOUT}}));
                  $262.agent.leaving();
                }});
              `);
            }}

            const i32a = new Int32Array(
              new SharedArrayBuffer(Int32Array.BYTES_PER_ELEMENT * BUFFER_SIZE)
            );
            $262.agent.safeBroadcast(i32a);
            $262.agent.waitUntil(i32a, RUNNING, NUMAGENT);
            $262.agent.tryYield();
            assert.sameValue(Atomics.notify(i32a, 0, NOTIFYCOUNT), NOTIFYCOUNT);
            $262.agent.trySleep(TIMEOUT);

            const reports = [];
            for (var i = 0; i < NUMAGENT; i++) {{
              reports.push($262.agent.getReport());
            }}
            reports.sort();
            for (var i = 0; i < NOTIFYCOUNT; i++) {{
              assert.sameValue(reports[i], 'ok');
            }}
            for (var i = NOTIFYCOUNT; i < NUMAGENT; i++) {{
              assert.sameValue(reports[i], 'timed-out');
            }}
            "pass";
            "#,
            D262_HOST_PREAMBLE, TEST262_STA, TEST262_ASSERT, TEST262_ATOMICS_HELPER
        );
        let completion = runtime
            .run_script(SourceInput::from_javascript(source), "<agent-helper-test>")
            .expect("script")
            .completion_string()
            .to_string();
        reset_for_next_test();
        assert_eq!(completion, "pass");
    }
}
