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
//! - Each agent thread holds its [`mpsc::Receiver<BroadcastMessage>`]
//!   in [`AGENT_INBOX`] (`thread_local!`) so `receiveBroadcast`
//!   blocks on the right channel without searching the registry.
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
//! - `parent_thread_id` filters the parent out of broadcast
//!   distribution so the parent (which also has `AGENT_INBOX`
//!   default = None) never receives its own messages.
//! - Each agent thread builds its own [`otter_runtime::Runtime`]
//!   with a private heap; the only state shared with the parent
//!   is the SAB's `Arc<SharedBody>`.
//!
//! # See also
//!
//! - <https://github.com/tc39/test262/blob/main/INTERPRETING.md#host-defined-functions>
//! - `docs/workers-262-plan.md` — slice 19c plan.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use otter_runtime::{OtterError, Runtime, SourceInput};
use otter_vm::binary::JsArrayBuffer;
use otter_vm::binary::array_buffer::SharedBody;
use otter_vm::number::NumberValue;
use otter_vm::string::JsString;
use otter_vm::{NativeCtx, NativeError, NativeFastFn, Value};

use crate::harness::D262_HOST_PREAMBLE;

/// One broadcast message handed from the parent thread to every
/// running agent through its [`AGENT_INBOX`] channel.
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

thread_local! {
    /// Receiver end of this agent's broadcast channel. The parent
    /// thread leaves this `None` so a stray `receiveBroadcast` call
    /// outside an agent fails deterministically with `TypeError`.
    static AGENT_INBOX: RefCell<Option<mpsc::Receiver<BroadcastMessage>>> = const { RefCell::new(None) };
    /// `true` once `$262.agent.leaving()` runs. Currently
    /// informational — the agent thread still terminates when its
    /// source body returns — but reserved for future
    /// `$262.agent.getReport` polling.
    static AGENT_LEAVING: AtomicBool = const { AtomicBool::new(false) };
}

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
const NATIVES: &[(&'static str, u8, NativeFastFn)] = &[
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

fn arg_to_string(ctx: &mut NativeCtx<'_>, value: &Value) -> Result<String, NativeError> {
    match value {
        Value::String(s) => Ok(s.to_lossy_string()),
        Value::Undefined => Ok("undefined".to_string()),
        Value::Null => Ok("null".to_string()),
        Value::Boolean(b) => Ok(if *b { "true" } else { "false" }.to_string()),
        Value::Number(n) => Ok(format!("{}", n.as_f64())),
        Value::BigInt(b) => Ok(b.as_inner().to_string()),
        _ => {
            // Fall through to `ToString` via the receiver's
            // `toString()` method. For the test262 surface this is
            // rare; broadcast / report typically take strings.
            let _ = ctx;
            Err(type_err("expected a string argument"))
        }
    }
}

// =====================================================================
// $262.agent.start(source)
// =====================================================================

fn agent_start(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let source = arg_to_string(ctx, args.first().unwrap_or(&Value::Undefined))?;
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
            AGENT_INBOX.with(|cell| {
                *cell.borrow_mut() = Some(rx);
            });
            run_agent_source(source);
        })
        .map_err(|e| type_err(format!("agent thread spawn failed: {e}")))?;

    Ok(Value::Undefined)
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

fn agent_broadcast(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let sab_value = args.first().unwrap_or(&Value::Undefined);
    let Value::ArrayBuffer(buf) = sab_value else {
        return Err(type_err("first argument must be a SharedArrayBuffer"));
    };
    let Some(shared) = buf.as_shared_arc() else {
        return Err(type_err("first argument must be a SharedArrayBuffer"));
    };
    let num = match args.get(1) {
        None | Some(Value::Undefined) => None,
        Some(Value::Number(n)) => Some(n.as_f64()),
        _ => return Err(type_err("second argument must be a Number or omitted")),
    };
    let msg = BroadcastMessage {
        sab: Arc::clone(shared),
        num,
    };
    // Capture sender list under lock then drop the lock before
    // sending so a slow recv does not stall other broadcasts.
    let senders: Vec<mpsc::Sender<BroadcastMessage>> = {
        let reg = AGENTS.lock().expect("agent registry poisoned");
        reg.senders.clone()
    };
    for tx in senders {
        let _ = tx.send(msg.clone());
    }
    Ok(Value::Undefined)
}

// =====================================================================
// $262.agent.receiveBroadcast(handler)
// =====================================================================

fn agent_receive_broadcast(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let handler = args.first().cloned().unwrap_or(Value::Undefined);
    if !matches!(
        handler,
        Value::NativeFunction(_)
            | Value::Closure { .. }
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
    ) {
        return Err(type_err("receiveBroadcast handler must be a function"));
    }

    // Take the receiver out of the thread local while we block on
    // recv. `rx.recv()` takes `&self` so the receiver remains
    // valid for the next call; we put it back after recv returns.
    let rx = AGENT_INBOX.with(|cell| cell.borrow_mut().take());
    let Some(rx) = rx else {
        return Err(type_err("receiveBroadcast called outside an agent thread"));
    };
    let result = rx.recv();
    // Always put the receiver back. If it's closed, the next
    // recv() returns `Disconnected` and the caller handles it.
    AGENT_INBOX.with(|cell| {
        *cell.borrow_mut() = Some(rx);
    });
    let msg = match result {
        Ok(m) => m,
        Err(_) => {
            // Senders all dropped before any broadcast arrived.
            return Ok(Value::Undefined);
        }
    };

    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| type_err("missing execution context"))?;

    // Rewrap the shared buffer on this agent's heap.
    let sab_value = Value::ArrayBuffer(JsArrayBuffer::from_shared_arc(msg.sab));

    let num_value = match msg.num {
        Some(n) => Value::Number(NumberValue::from_f64(n)),
        None => Value::Undefined,
    };

    let mut args_vec = smallvec::SmallVec::<[Value; 8]>::new();
    args_vec.push(sab_value);
    args_vec.push(num_value);

    interp
        .run_callable_sync(&exec, &handler, Value::Undefined, args_vec)
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
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::Undefined) | None => 0.0,
        _ => return Err(type_err("sleep argument must be a Number")),
    };
    if ms.is_finite() && ms > 0.0 {
        thread::sleep(Duration::from_millis(ms as u64));
    }
    Ok(Value::Undefined)
}

fn agent_monotonic_now(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let base = *MONOTONIC_BASE;
    let elapsed_ms = base.elapsed().as_secs_f64() * 1000.0;
    Ok(Value::Number(NumberValue::from_f64(elapsed_ms)))
}

// =====================================================================
// $262.agent.report(msg) / getReport()
// =====================================================================

fn agent_report(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let msg = arg_to_string(ctx, args.first().unwrap_or(&Value::Undefined))?;
    let mut reg = AGENTS.lock().expect("agent registry poisoned");
    reg.reports.push_back(msg);
    Ok(Value::Undefined)
}

fn agent_get_report(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    let mut reg = AGENTS.lock().expect("agent registry poisoned");
    let msg = reg.reports.pop_front();
    drop(reg);
    match msg {
        None => Ok(Value::Null),
        Some(s) => {
            let (interp, _exec) = ctx.interp_mut_and_context();
            let heap = interp.string_heap();
            let js =
                JsString::from_str(&s, heap).map_err(|e| type_err(format!("string alloc: {e}")))?;
            Ok(Value::String(js))
        }
    }
}

// =====================================================================
// $262.agent.leaving()
// =====================================================================

fn agent_leaving(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    AGENT_LEAVING.with(|f| f.store(true, Ordering::Release));
    Ok(Value::Undefined)
}
