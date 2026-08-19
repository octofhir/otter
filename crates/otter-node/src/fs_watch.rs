//! Native file watching under `fs.watch` and `fs.watchFile`.
//!
//! # Contents
//! - [`fs_watch_binding_cjs_value`] exports the surface as
//!   `internal/otter/fs_watch` for the compat `fs_event_wrap` handle and
//!   the compat `StatWatcher`.
//! - A watcher table owned by the natives, keyed by the handle JavaScript
//!   holds.
//!
//! # Invariants
//! - A path is watched only when the read capability allows it: a change
//!   notification tells the program the file exists and when it moved.
//! - Notifications reach JavaScript on the isolate thread, in order and
//!   without dropping, the way datagrams do.
//! - A persistent watcher holds the loop open; `unref` releases that hold
//!   without stopping the watch.
//! - `fs.watchFile` polls, and reports a change only when the stat it takes
//!   differs from the one before it, which is what a caller comparing the
//!   two `Stats` it receives expects.
//!
//! # See also
//! - `nodelib/compat/internal_fs_event_wrap.js`
//! - `nodelib/compat/internal_fs_binding.js` — the `StatWatcher` half.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use notify::{RecursiveMode, Watcher};
use otter_runtime::{
    CapabilitySet, OtterError, Runtime, RuntimeKeepAlive, RuntimeLiveness, RuntimeLocal,
    RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope, RuntimeTask, RuntimeTaskSpawner,
    RuntimeValue, runtime_arg_to_string,
};

/// The uv error number for an absent path, as JavaScript reads it back off
/// a failed `start`.
const UV_ENOENT: f64 = -2.0;
/// The uv error number for a path the capability gate refuses.
const UV_EACCES: f64 = -13.0;

/// One live watcher: whatever the host needs to keep watching, plus the
/// loop hold that makes the program wait for it.
struct WatchEntry {
    /// Dropped to stop a `fs.watch` notification stream.
    _watcher: Option<Box<dyn Watcher + Send>>,
    /// Set false to stop a `fs.watchFile` poll.
    running: Arc<std::sync::atomic::AtomicBool>,
    keep_alive: Option<RuntimeKeepAlive>,
}

type WatchTable = Arc<Mutex<HashMap<u32, WatchEntry>>>;

/// The watch surface the compat handles drive, exported as
/// `internal/otter/fs_watch`.
///
/// # Errors
/// Returns a native error when the surface fails to allocate.
pub fn fs_watch_binding_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    capabilities: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: RuntimeLocal<'scope>,
    _require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let watchers: WatchTable = Arc::new(Mutex::new(HashMap::new()));
    let next_id = Arc::new(AtomicU32::new(1));

    {
        let caps = capabilities.clone();
        let spawner = runtime_task_spawner.clone();
        let table = watchers.clone();
        let ids = next_id.clone();
        let start = scope.native_closure(
            "watch",
            3,
            &[],
            move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
                start_watch(ctx, args, &caps, spawner.as_ref(), &table, &ids)
            },
        )?;
        scope.set(object, "watch", start)?;
    }

    {
        let caps = capabilities.clone();
        let spawner = runtime_task_spawner.clone();
        let table = watchers.clone();
        let ids = next_id.clone();
        let start = scope.native_closure(
            "watchFile",
            3,
            &[],
            move |ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
                start_poll(ctx, args, &caps, spawner.as_ref(), &table, &ids)
            },
        )?;
        scope.set(object, "watchFile", start)?;
    }

    {
        let table = watchers.clone();
        let stop = scope.native_closure(
            "close",
            1,
            &[],
            move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
                let id = handle_arg(args, 0);
                if let Some(entry) = table
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .remove(&id)
                {
                    entry.running.store(false, Ordering::Relaxed);
                }
                Ok(RuntimeValue::undefined())
            },
        )?;
        scope.set(object, "close", stop)?;
    }

    {
        let table = watchers.clone();
        let hold = scope.native_closure(
            "hold",
            2,
            &[],
            move |_ctx: &mut RuntimeNativeCtx<'_>, args: &[RuntimeValue], _c: &[RuntimeValue]| {
                let id = handle_arg(args, 0);
                let referenced = args
                    .get(1)
                    .and_then(|value| value.as_boolean())
                    .unwrap_or(true);
                let table = table
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(entry) = table.get(&id)
                    && let Some(keep_alive) = &entry.keep_alive
                {
                    if referenced {
                        keep_alive.ref_();
                    } else {
                        keep_alive.unref();
                    }
                }
                Ok(RuntimeValue::undefined())
            },
        )?;
        scope.set(object, "hold", hold)?;
    }

    Ok(object)
}

fn handle_arg(args: &[RuntimeValue], index: usize) -> u32 {
    args.get(index)
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as u32
}

/// `watch(path, recursive, persistent)` — answers the handle, or a negative
/// uv error number the caller reports as a failed `start`.
fn start_watch(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    spawner: Option<&RuntimeTaskSpawner>,
    watchers: &WatchTable,
    next_id: &AtomicU32,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = PathBuf::from(runtime_arg_to_string(args, 0, ctx.heap()));
    let recursive = args
        .get(1)
        .and_then(|value| value.as_boolean())
        .unwrap_or(false);
    let persistent = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(true);
    if !capabilities.read.matches_path(&path) {
        return Ok(RuntimeValue::number_f64(UV_EACCES));
    }
    let Ok(metadata) = std::fs::metadata(&path) else {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    };
    let Some(spawner) = spawner else {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    };
    let Some(io) = spawner.io_handle() else {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    };

    // Watching a file reports that one name; watching a directory reports
    // names relative to it, which is the path the notification carries with
    // the watched directory cut off the front.
    let root = if metadata.is_dir() {
        Some(path.clone())
    } else {
        None
    };
    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<Notification>();

    let mut watcher =
        match notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            let Ok(event) = event else { return };
            for notification in notifications(id, &event, root.as_deref()) {
                // The receiver is dropped when the watch is closed; nothing
                // is left to deliver then.
                let _ = sender.send(notification);
            }
        }) {
            Ok(watcher) => watcher,
            Err(_) => return Ok(RuntimeValue::number_f64(UV_ENOENT)),
        };
    let mode = if recursive {
        RecursiveMode::Recursive
    } else {
        RecursiveMode::NonRecursive
    };
    if watcher.watch(&path, mode).is_err() {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    }

    let keep_alive = spawner.retain_keep_alive(if persistent {
        RuntimeLiveness::Ref
    } else {
        RuntimeLiveness::Unref
    });
    watchers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            WatchEntry {
                _watcher: Some(Box::new(watcher)),
                running: Arc::new(std::sync::atomic::AtomicBool::new(true)),
                keep_alive: Some(keep_alive),
            },
        );

    let delivery = spawner.clone();
    io.spawn(async move {
        while let Some(notification) = receiver.recv().await {
            if !delivery
                .enqueue_ordered(notification, RuntimeLiveness::Unref)
                .await
            {
                return;
            }
        }
    });

    Ok(RuntimeValue::number_f64(f64::from(id)))
}

/// The change notifications one host event stands for.
fn notifications(id: u32, event: &notify::Event, root: Option<&Path>) -> Vec<Notification> {
    // A name that appears, disappears or moves is a rename; anything else
    // that touches the bytes or the metadata is a change. That is the split
    // `fs.watch` reports and all it reports.
    let kind = match event.kind {
        notify::EventKind::Create(_) | notify::EventKind::Remove(_) => "rename",
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => "rename",
        notify::EventKind::Modify(_) => "change",
        _ => return Vec::new(),
    };
    event
        .paths
        .iter()
        .map(|path| {
            let name = match root {
                Some(root) => path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned(),
                None => path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            Notification {
                id,
                kind,
                filename: name,
            }
        })
        .collect()
}

/// `watchFile(path, intervalMs, persistent)` — answers the handle.
fn start_poll(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
    capabilities: &CapabilitySet,
    spawner: Option<&RuntimeTaskSpawner>,
    watchers: &WatchTable,
    next_id: &AtomicU32,
) -> Result<RuntimeValue, RuntimeNativeError> {
    let path = PathBuf::from(runtime_arg_to_string(args, 0, ctx.heap()));
    let interval = args
        .get(1)
        .and_then(|value| value.as_f64())
        .unwrap_or(5007.0)
        .max(1.0);
    let persistent = args
        .get(2)
        .and_then(|value| value.as_boolean())
        .unwrap_or(true);
    if !capabilities.read.matches_path(&path) {
        return Ok(RuntimeValue::number_f64(UV_EACCES));
    }
    let Some(spawner) = spawner else {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    };
    let Some(io) = spawner.io_handle() else {
        return Ok(RuntimeValue::number_f64(UV_ENOENT));
    };

    let id = next_id.fetch_add(1, Ordering::Relaxed);
    let running = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let keep_alive = spawner.retain_keep_alive(if persistent {
        RuntimeLiveness::Ref
    } else {
        RuntimeLiveness::Unref
    });
    watchers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(
            id,
            WatchEntry {
                _watcher: None,
                running: running.clone(),
                keep_alive: Some(keep_alive),
            },
        );

    let delivery = spawner.clone();
    io.spawn(async move {
        // The stat the poll starts from is the baseline: a caller learns
        // about the changes after it started watching, not about the state
        // it already asked for.
        let mut previous = stat_slots(&path);
        loop {
            tokio::time::sleep(std::time::Duration::from_millis(interval as u64)).await;
            if !running.load(Ordering::Relaxed) {
                return;
            }
            let current = stat_slots(&path);
            let changed = match (&previous, &current) {
                (Some(before), Some(now)) => before != now,
                (None, None) => false,
                _ => true,
            };
            if !changed {
                continue;
            }
            let status = if current.is_some() { 0.0 } else { UV_ENOENT };
            let poll = PollResult {
                id,
                status,
                current: current.unwrap_or([0.0; SLOTS]),
                previous: previous.unwrap_or([0.0; SLOTS]),
            };
            previous = current;
            if !delivery.enqueue_ordered(poll, RuntimeLiveness::Unref).await {
                return;
            }
        }
    });

    Ok(RuntimeValue::number_f64(f64::from(id)))
}

/// The number of slots one stat occupies, matching the layout
/// `getStatsFromBinding` reads.
const SLOTS: usize = 18;

/// One stat, in the layout the binding hands to JavaScript, or nothing when
/// the path could not be stat'd.
fn stat_slots(path: &Path) -> Option<[f64; SLOTS]> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).ok()?;
    Some([
        metadata.dev() as f64,
        f64::from(metadata.mode()),
        metadata.nlink() as f64,
        f64::from(metadata.uid()),
        f64::from(metadata.gid()),
        metadata.rdev() as f64,
        metadata.blksize() as f64,
        metadata.ino() as f64,
        metadata.size() as f64,
        metadata.blocks() as f64,
        metadata.atime() as f64,
        metadata.atime_nsec() as f64,
        metadata.mtime() as f64,
        metadata.mtime_nsec() as f64,
        metadata.ctime() as f64,
        metadata.ctime_nsec() as f64,
        metadata.created().map_or(0.0, |time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |since| since.as_secs() as f64)
        }),
        metadata.created().map_or(0.0, |time| {
            time.duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |since| f64::from(since.subsec_nanos()))
        }),
    ])
}

/// One `fs.watch` notification, handed to the isolate thread.
#[derive(Clone)]
struct Notification {
    id: u32,
    kind: &'static str,
    filename: String,
}

impl RuntimeTask for Notification {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatch = scope.get(globals, "__otterFsWatchDeliver")?;
                if !scope.is_callable(dispatch) {
                    return Ok(RuntimeValue::undefined());
                }
                let id = scope.number(f64::from(self.id));
                let kind = scope.string(self.kind)?;
                let filename = scope.string(&self.filename)?;
                let undefined = scope.undefined();
                let result = scope.call(dispatch, undefined, &[id, kind, filename])?;
                Ok(scope.finish(result))
            })
        })
    }
}

/// One `fs.watchFile` poll that saw a change, handed to the isolate thread.
#[derive(Clone)]
struct PollResult {
    id: u32,
    status: f64,
    current: [f64; SLOTS],
    previous: [f64; SLOTS],
}

impl RuntimeTask for PollResult {
    fn run(self: Box<Self>, runtime: &mut Runtime) -> Result<(), OtterError> {
        let Some(context) = runtime.realm_execution_context() else {
            return Ok(());
        };
        runtime.run_native_event(&context, |ctx| {
            ctx.scope(|mut scope| {
                let globals = scope.global_this();
                let dispatch = scope.get(globals, "__otterFsWatchFileDeliver")?;
                if !scope.is_callable(dispatch) {
                    return Ok(RuntimeValue::undefined());
                }
                let id = scope.number(f64::from(self.id));
                let status = scope.number(self.status);
                // Current stat first, the one before it after: the pair the
                // caller's `(curr, prev)` listener is handed.
                let slots = scope.array(SLOTS * 2)?;
                for (index, value) in self.current.iter().chain(self.previous.iter()).enumerate() {
                    let number = scope.number(*value);
                    scope.set_index(slots, index, number)?;
                }
                let undefined = scope.undefined();
                let result = scope.call(dispatch, undefined, &[id, status, slots])?;
                Ok(scope.finish(result))
            })
        })
    }
}
