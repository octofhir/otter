//! `$262.agent.*` host harness (slice 19c).
//!
//! Test262 cross-worker tests require a host-defined agent model
//! (start agent threads, broadcast a `SharedArrayBuffer`, wake up
//! `Atomics.wait` waiters from another thread, etc.). This module
//! plugs that surface into the test262 runner.
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
//! - Each agent thread builds its own [`otter_runtime::Runtime`]
//!   with a private heap; the only state shared with the parent
//!   is the SAB's `Arc<SharedBody>`.
//!
//! # See also
//!
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md#host-defined-functions>
//! - `docs/workers-262-plan.md` — slice 19c plan.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use otter_runtime::{OtterError, Runtime, SourceInput};
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
    /// `$262.agent.report` / `$262.agent.getReport` FIFO.
    reports: VecDeque<String>,
}

static AGENTS: LazyLock<Mutex<AgentRegistry>> = LazyLock::new(|| {
    Mutex::new(AgentRegistry {
        senders: Vec::new(),
        reports: VecDeque::new(),
    })
});

/// Receiver end for each live agent thread. The parent thread never
/// inserts an inbox, so a stray `receiveBroadcast` call outside an
/// agent fails deterministically with `TypeError`.
static AGENT_INBOXES: LazyLock<Mutex<HashMap<thread::ThreadId, mpsc::Receiver<BroadcastMessage>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Monotonic clock anchor — `monotonicNow()` returns
/// milliseconds since the first call inside the process.
static MONOTONIC_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Reset the cross-agent shared state. Called from the per-test
/// driver between tests so a previous test's leftover agents /
/// reports do not bleed into the next.
pub fn reset_for_next_test() {
    let mut reg = AGENTS.lock().expect("agent registry poisoned");
    reg.senders.clear();
    reg.reports.clear();
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .clear();
}

/// Install every `__otter_agent_*` native global on the given
/// runtime. The runner calls this once per fresh runtime before
/// the harness preamble runs so the JS-side `$262.agent` object
/// in [`D262_HOST_PREAMBLE`] resolves to live bindings.
pub fn install_natives(runtime: &mut Runtime) -> Result<(), OtterError> {
    for (name, length, call) in NATIVES {
        runtime.install_native_global(name, *length, *call)?;
    }
    Ok(())
}

/// `(name, length, fn)` table installed by [`install_natives`].
const NATIVES: &[(&str, u8, NativeFastFn)] = &[
    ("__otter_is_htmldda", 0, is_html_dda),
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

    {
        let mut reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.push(tx);
    }

    // Spawn the agent on a real OS thread. The thread builds its
    // own runtime; the only Rust state shared with the parent is
    // the `Arc<SharedBody>` that rides through the channel.
    thread::Builder::new()
        .name("test262-agent".to_string())
        .spawn(move || {
            let thread_id = thread::current().id();
            AGENT_INBOXES
                .lock()
                .expect("agent inbox registry poisoned")
                .insert(thread_id, rx);
            run_agent_source(source);
            AGENT_INBOXES
                .lock()
                .expect("agent inbox registry poisoned")
                .remove(&thread_id);
        })
        .map_err(|e| type_err(format!("agent thread spawn failed: {e}")))?;

    Ok(Value::undefined())
}

fn run_agent_source(source: String) {
    // Build the same preamble the runner uses for the main thread
    // so the agent observes a full `$262` global.
    let mut combined = String::with_capacity(D262_HOST_PREAMBLE.len() + source.len() + 16);
    combined.push_str(D262_HOST_PREAMBLE);
    combined.push('\n');
    combined.push_str(&source);

    let Ok(mut runtime) = Runtime::builder()
        .timeout(Duration::from_secs(30))
        .max_heap_bytes(256 * 1024 * 1024)
        .build()
    else {
        return;
    };
    if install_natives(&mut runtime).is_err() {
        return;
    }
    let _ = runtime.run_script(SourceInput::from_javascript(combined), "test262-agent");
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
    let result = rx.recv();
    // Always put the receiver back. If it's closed, the next
    // recv() returns `Disconnected` and the caller handles it.
    AGENT_INBOXES
        .lock()
        .expect("agent inbox registry poisoned")
        .insert(thread_id, rx);
    let msg = match result {
        Ok(m) => m,
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

    interp
        .run_callable_sync(&exec, &handler, Value::undefined(), args_vec)
        .map_err(|e| match e {
            otter_vm::VmError::Uncaught { value } => NativeError::Thrown {
                name: "$262.agent.receiveBroadcast",
                message: value,
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
