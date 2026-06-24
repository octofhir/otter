//! `$262.agent.*` host harness (slice 19c).
//!
//! Test262 cross-agent tests require a host-defined agent model
//! (start agents, broadcast a `SharedArrayBuffer`, wake up
//! `Atomics.wait` waiters from another isolate, etc.). This module
//! plugs that surface into the test262 runner by routing
//! `$262.agent.start` through the real runtime Worker backend.
//!
//! Architecture:
//!
//! - One [`AgentRegistry`] lives in a process-wide `LazyLock`. It
//!   owns:
//!   * one `mpsc::Sender<BroadcastMessage>` per started agent so
//!     the parent thread's `$262.agent.broadcast` can fan out to
//!     every running agent;
//!   * a FIFO `VecDeque<String>` for `$262.agent.report` /
//!     `$262.agent.getReport`.
//! - Agent inboxes are keyed by [`thread::ThreadId`] in
//!   [`AGENT_INBOXES`] so `receiveBroadcast` blocks on the right
//!   channel without thread-local state.
//! - The shared buffer rides through the channel as an
//!   `Arc<SharedBody>`. The receiving agent rewraps it via
//!   [`JsArrayBuffer::from_shared_arc`] before handing it to the
//!   user handler, so both sides observe the same backing storage
//!   and the same `Atomics.wait` registry id.
//!
//! Hardening:
//!
//! - The parent thread captures `Vec<Sender>` then releases the
//!   registry lock **before** sending — agents may block on recv,
//!   and holding the registry lock across a blocking send would
//!   serialise broadcast dispatch.
//! - The parent never registers an inbox, so it never receives its
//!   own broadcast messages.
//! - Each agent is a real Worker runtime with a private heap; the
//!   only state shared with the parent is the SAB's
//!   `Arc<SharedBody>`.
//!
//! # See also
//!
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md#host-defined-functions>
//! - `docs/workers-262-plan.md` — slice 19c plan.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use otter_runtime::{InterruptHandle, OtterError, Runtime};
use otter_vm::binary::JsArrayBuffer;
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
    /// Worker ids returned by the runtime Worker backend.
    worker_ids: Vec<u64>,
    /// `$262.agent.report` / `$262.agent.getReport` FIFO.
    reports: VecDeque<String>,
}

static AGENTS: LazyLock<Mutex<AgentRegistry>> = LazyLock::new(|| {
    Mutex::new(AgentRegistry {
        senders: Vec::new(),
        handles: Vec::new(),
        interrupts: Vec::new(),
        worker_ids: Vec::new(),
        reports: VecDeque::new(),
    })
});

/// Receiver end for each live agent thread. The parent thread never
/// inserts an inbox, so a stray `receiveBroadcast` call outside an
/// agent fails deterministically with `TypeError`.
static AGENT_INBOXES: LazyLock<Mutex<HashMap<thread::ThreadId, mpsc::Receiver<BroadcastMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static PENDING_WORKER_INBOXES: LazyLock<Mutex<VecDeque<mpsc::Receiver<BroadcastMessage>>>> =
    LazyLock::new(|| Mutex::new(VecDeque::new()));
static AGENT_TEMP_FILES: LazyLock<Mutex<Vec<PathBuf>>> = LazyLock::new(|| Mutex::new(Vec::new()));
static NEXT_AGENT_FILE_ID: AtomicU64 = AtomicU64::new(1);
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
        reg.worker_ids.clear();
        (
            std::mem::take(&mut reg.handles),
            std::mem::take(&mut reg.interrupts),
        )
    };
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .clear();
    PENDING_WORKER_INBOXES
        .lock()
        .expect("agent pending inbox registry poisoned")
        .clear();
    for interrupt in interrupts {
        interrupt.interrupt();
    }
    otter_vm::atomics_wait::cancel_all_waiters();
    for handle in handles {
        let _ = handle.join();
    }
    let paths = AGENT_TEMP_FILES
        .lock()
        .expect("agent temp-file registry poisoned")
        .drain(..)
        .collect::<Vec<_>>();
    for path in paths {
        let _ = fs::remove_file(path);
    }
}

/// Install every `__otter_agent_*` native global on the given
/// runtime. The runner calls this once per fresh runtime before
/// the harness preamble runs so the JS-side `$262.agent` object
/// in [`D262_HOST_PREAMBLE`] resolves to live bindings.
pub fn install_natives(runtime: &mut Runtime) -> Result<(), OtterError> {
    claim_pending_worker_inbox();
    for (name, length, call) in NATIVES {
        runtime.install_native_global(name, *length, *call)?;
    }
    Ok(())
}

/// `(name, length, fn)` table installed by [`install_natives`].
const NATIVES: &[(&str, u8, NativeFastFn)] = &[
    ("__otter_is_htmldda", 0, is_html_dda),
    ("__otter_eval_script", 1, eval_script),
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

/// `$262.evalScript(source)` — INTERPRETING.md host API: parse
/// `source` as an ECMAScript Script and run it in the current realm
/// with §16.1.7 GlobalDeclarationInstantiation semantics.
fn eval_script(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let result = ctx.interp_mut().run_host_script(&arg);
    let detail = ctx.interp_mut().take_error_detail();
    let detail_msg = match &detail {
        Some(otter_vm::ErrorDetail::Message(m))
        | Some(otter_vm::ErrorDetail::Name(m))
        | Some(otter_vm::ErrorDetail::Uncaught(m)) => m.to_string(),
        _ => String::new(),
    };
    result.map_err(|err| match err {
        otter_vm::VmError::SyntaxError => NativeError::SyntaxError {
            name: "evalScript",
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
                Some(0) => NativeError::SyntaxError {
                    name: "evalScript",
                    reason,
                },
                Some(2) => NativeError::ReferenceError {
                    name: "evalScript",
                    reason,
                },
                Some(3) => NativeError::RangeError {
                    name: "evalScript",
                    reason,
                },
                _ => NativeError::TypeError {
                    name: "evalScript",
                    reason: render,
                },
            }
        }
        err => NativeError::TypeError {
            name: "evalScript",
            reason: err.to_string(),
        },
    })
}

fn claim_pending_worker_inbox() {
    let rx = PENDING_WORKER_INBOXES
        .lock()
        .expect("agent pending inbox registry poisoned")
        .pop_front();
    if let Some(rx) = rx {
        AGENT_INBOXES
            .lock()
            .expect("agent inbox registry poisoned")
            .insert(thread::current().id(), rx);
    }
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

    let entry = write_agent_entry(source)?;
    PENDING_WORKER_INBOXES
        .lock()
        .expect("agent pending inbox registry poisoned")
        .push_back(rx);
    let worker_id = spawn_worker_agent(ctx, &entry)?;
    {
        let mut reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.push(tx);
        reg.worker_ids.push(worker_id);
    }

    Ok(Value::undefined())
}

fn write_agent_entry(source: String) -> Result<PathBuf, NativeError> {
    let mut combined = String::with_capacity(D262_HOST_PREAMBLE.len() + source.len() + 16);
    combined.push_str(D262_HOST_PREAMBLE);
    combined.push('\n');
    combined.push_str(&source);

    let id = NEXT_AGENT_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let path = std::env::temp_dir().join(format!("otter-test262-agent-{id}.js"));
    fs::write(&path, combined)
        .map_err(|err| type_err(format!("agent entry write failed: {err}")))?;
    AGENT_TEMP_FILES
        .lock()
        .expect("agent temp-file registry poisoned")
        .push(path.clone());
    Ok(path)
}

fn spawn_worker_agent(
    ctx: &mut NativeCtx<'_>,
    entry: &std::path::Path,
) -> Result<u64, NativeError> {
    let path = entry.to_string_lossy().to_string();
    let path_value = {
        let js = JsString::from_str(&path, ctx.heap_mut())
            .map_err(|err| type_err(format!("agent path string allocation failed: {err}")))?;
        Value::string(js)
    };
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| type_err("missing execution context"))?;
    let spawn = otter_vm::object::get(
        *interp.global_this(),
        interp.gc_heap(),
        "__otter_worker_spawn",
    )
    .ok_or_else(|| type_err("Worker backend is not installed"))?;
    let result = interp.run_callable_sync(
        &exec,
        &spawn,
        Value::undefined(),
        smallvec::smallvec![path_value],
    );
    let uncaught_msg = match interp.take_error_detail() {
        Some(otter_vm::ErrorDetail::Uncaught(m)) => m.to_string(),
        _ => String::new(),
    };
    match result {
        Ok(value) => value
            .as_number()
            .map(|number| number.as_f64() as u64)
            .ok_or_else(|| type_err("Worker backend returned a non-numeric id")),
        Err(otter_vm::VmError::Uncaught) => Err(NativeError::Thrown {
            name: "$262.agent.start",
            message: uncaught_msg,
        }),
        Err(other) => Err(type_err(other.to_string())),
    }
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
    let interrupt = ctx.interp_mut().interrupt_handle();
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

    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| type_err("missing execution context"))?;

    // Rewrap the shared buffer on this agent's heap.
    let sab_handle = JsArrayBuffer::from_shared_arc(interp.gc_heap_mut(), msg.sab)
        .map_err(|_| type_err("out of memory while wrapping SharedArrayBuffer"))?;
    let sab_value = Value::array_buffer(sab_handle);

    let num_value = match msg.num {
        Some(n) => Value::number_f64(n),
        None => Value::undefined(),
    };

    let mut args_vec = smallvec::SmallVec::<[Value; 8]>::new();
    args_vec.push(sab_value);
    args_vec.push(num_value);

    let result = interp.run_callable_sync(&exec, &handler, Value::undefined(), args_vec);
    let uncaught_msg = match interp.take_error_detail() {
        Some(otter_vm::ErrorDetail::Uncaught(m)) => m.to_string(),
        _ => String::new(),
    };
    result.map_err(|e| match e {
        otter_vm::VmError::Uncaught => NativeError::Thrown {
            name: "$262.agent.receiveBroadcast",
            message: uncaught_msg,
        },
        other => type_err(other.to_string()),
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
    drain_worker_agent_events(ctx)?;
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

fn drain_worker_agent_events(ctx: &mut NativeCtx<'_>) -> Result<(), NativeError> {
    let worker_ids = {
        let reg = AGENTS.lock().expect("agent registry poisoned");
        reg.worker_ids.clone()
    };
    if worker_ids.is_empty() {
        return Ok(());
    }
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| type_err("missing execution context"))?;
    let Some(drain) = otter_vm::object::get(
        *interp.global_this(),
        interp.gc_heap(),
        "__otter_worker_drain",
    ) else {
        return Ok(());
    };
    let mut surfaced = Vec::new();
    for id in worker_ids {
        let drain_result = interp.run_callable_sync(
            &exec,
            &drain,
            Value::undefined(),
            smallvec::smallvec![Value::number_f64(id as f64)],
        );
        let uncaught_msg = match interp.take_error_detail() {
            Some(otter_vm::ErrorDetail::Uncaught(m)) => m.to_string(),
            _ => String::new(),
        };
        let events = drain_result.map_err(|err| match err {
            otter_vm::VmError::Uncaught => NativeError::Thrown {
                name: "$262.agent.getReport",
                message: uncaught_msg,
            },
            other => type_err(other.to_string()),
        })?;
        let Some(events) = events.as_array() else {
            continue;
        };
        let len = otter_vm::array::len(events, interp.gc_heap());
        for idx in 0..len {
            let event = otter_vm::array::get(events, interp.gc_heap(), idx);
            let Some(event) = event.as_object() else {
                continue;
            };
            let ty = otter_vm::object::get(event, interp.gc_heap(), "type")
                .and_then(|value| value.as_string(interp.gc_heap()))
                .map(|value| value.to_lossy_string(interp.gc_heap()))
                .unwrap_or_default();
            if ty == "error" || ty == "messageerror" {
                let message = otter_vm::object::get(event, interp.gc_heap(), "message")
                    .and_then(|value| value.as_string(interp.gc_heap()))
                    .map(|value| value.to_lossy_string(interp.gc_heap()))
                    .unwrap_or_else(|| ty.clone());
                surfaced.push(format!("agent {ty}: {message}"));
            }
        }
    }
    if !surfaced.is_empty() {
        let mut reg = AGENTS.lock().expect("agent registry poisoned");
        reg.reports.extend(surfaced);
    }
    Ok(())
}

// =====================================================================
// $262.agent.leaving()
// =====================================================================

fn agent_leaving(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}
