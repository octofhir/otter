//! Change notifications on macOS from FSEvents, with CoreServices opened by
//! the first watcher instead of linked into the binary.
//!
//! # Contents
//! - [`FsEventWatcher`] — a `notify::Watcher` over one FSEvents stream.
//!
//! # Invariants
//! - CoreServices, and the CoreFoundation it brings, is opened with `dlopen`
//!   when a watcher is made: a process that watches nothing loads neither,
//!   so a launch pays no framework start-up for watching it never does.
//! - A stream delivers on its own serial dispatch queue, and is stopped,
//!   invalidated and released on that queue: once `stop` returns no callback
//!   runs, and the release callback has freed the context callbacks read.
//! - A stream's paths are fixed when it is made, so changing the watched set
//!   makes a new stream.
//! - A notification reaches the handler when its path is a watched path, or
//!   inside one watched recursively, or directly inside one watched flat.
//!
//! # See also
//! - [`crate::fs_watch`] — `fs.watch` on this watcher.
//! - <https://developer.apple.com/documentation/coreservices/file_system_events>

use std::ffi::{CStr, CString, c_char, c_void};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::event::{
    CreateKind, DataChange, Event, EventKind, Flag, MetadataKind, ModifyKind, RemoveKind,
    RenameMode,
};
use notify::{Config, Error, EventHandler, RecursiveMode, Result, Watcher, WatcherKind};

type CfRef = *const c_void;
type StreamRef = *mut c_void;
type DispatchQueue = *mut c_void;

type StreamCallback =
    extern "C" fn(StreamRef, *mut c_void, usize, *mut c_void, *const u32, *const u64);

/// `FSEventStreamContext`.
#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: Option<extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<extern "C" fn(*const c_void)>,
    copy_description: Option<extern "C" fn(*const c_void) -> CfRef>,
}

const SINCE_NOW: u64 = u64::MAX;
const CREATE_NO_DEFER: u32 = 0x2;
const CREATE_FILE_EVENTS: u32 = 0x10;

const MUST_SCAN_SUBDIRS: u32 = 0x1;
const USER_DROPPED: u32 = 0x2;
const KERNEL_DROPPED: u32 = 0x4;
const HISTORY_DONE: u32 = 0x10;
const ROOT_CHANGED: u32 = 0x20;
const MOUNT: u32 = 0x40;
const UNMOUNT: u32 = 0x80;
const ITEM_CREATED: u32 = 0x100;
const ITEM_REMOVED: u32 = 0x200;
const ITEM_INODE_META_MOD: u32 = 0x400;
const ITEM_RENAMED: u32 = 0x800;
const ITEM_MODIFIED: u32 = 0x1000;
const ITEM_FINDER_INFO_MOD: u32 = 0x2000;
const ITEM_CHANGE_OWNER: u32 = 0x4000;
const ITEM_XATTR_MOD: u32 = 0x8000;
const ITEM_IS_FILE: u32 = 0x10000;
const ITEM_IS_DIR: u32 = 0x20000;
const ITEM_IS_SYMLINK: u32 = 0x40000;
const OWN_EVENT: u32 = 0x80000;
const ITEM_IS_HARDLINK: u32 = 0x100000;
const ITEM_CLONED: u32 = 0x400000;

unsafe extern "C" {
    fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> DispatchQueue;
    fn dispatch_sync_f(
        queue: DispatchQueue,
        context: *mut c_void,
        work: extern "C" fn(*mut c_void),
    );
    fn dispatch_release(object: DispatchQueue);
}

type StringCreate = unsafe extern "C" fn(CfRef, *const c_char) -> CfRef;
type ArrayCreate = unsafe extern "C" fn(CfRef, *const CfRef, isize, *const c_void) -> CfRef;
type CfRelease = unsafe extern "C" fn(CfRef);
type StreamCreate = unsafe extern "C" fn(
    CfRef,
    StreamCallback,
    *const StreamContext,
    CfRef,
    u64,
    f64,
    u32,
) -> StreamRef;
type SetDispatchQueue = unsafe extern "C" fn(StreamRef, DispatchQueue);
type StreamStart = unsafe extern "C" fn(StreamRef) -> u8;
type StreamOp = unsafe extern "C" fn(StreamRef);

/// The CoreServices and CoreFoundation entry points a watcher calls.
struct Symbols {
    string_create: StringCreate,
    array_create: ArrayCreate,
    cf_release: CfRelease,
    array_callbacks: *const c_void,
    stream_create: StreamCreate,
    set_dispatch_queue: SetDispatchQueue,
    start: StreamStart,
    stop: StreamOp,
    invalidate: StreamOp,
    release: StreamOp,
}

impl Symbols {
    fn load() -> Result<Self> {
        const PATH: &CStr = c"/System/Library/Frameworks/CoreServices.framework/CoreServices";
        // SAFETY: the path is NUL-terminated; a framework stays loaded once
        // opened, so the handle is never closed and the symbols stay valid.
        let handle = unsafe { libc::dlopen(PATH.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL) };
        if handle.is_null() {
            return Err(Error::generic("CoreServices could not be loaded"));
        }
        let symbol = |name: &CStr| -> Result<*mut c_void> {
            // SAFETY: `handle` is a live `dlopen` handle and `name` is
            // NUL-terminated.
            let address = unsafe { libc::dlsym(handle, name.as_ptr()) };
            if address.is_null() {
                Err(Error::generic(&format!(
                    "CoreServices has no {}",
                    name.to_string_lossy()
                )))
            } else {
                Ok(address)
            }
        };
        let string_create = symbol(c"CFStringCreateWithFileSystemRepresentation")?;
        let array_create = symbol(c"CFArrayCreate")?;
        let cf_release = symbol(c"CFRelease")?;
        let array_callbacks = symbol(c"kCFTypeArrayCallBacks")?;
        let stream_create = symbol(c"FSEventStreamCreate")?;
        let set_dispatch_queue = symbol(c"FSEventStreamSetDispatchQueue")?;
        let start = symbol(c"FSEventStreamStart")?;
        let stop = symbol(c"FSEventStreamStop")?;
        let invalidate = symbol(c"FSEventStreamInvalidate")?;
        let release = symbol(c"FSEventStreamRelease")?;
        // SAFETY: each address is the named function, whose C signature the
        // field's type spells out.
        unsafe {
            Ok(Self {
                string_create: std::mem::transmute::<*mut c_void, StringCreate>(string_create),
                array_create: std::mem::transmute::<*mut c_void, ArrayCreate>(array_create),
                cf_release: std::mem::transmute::<*mut c_void, CfRelease>(cf_release),
                array_callbacks: array_callbacks.cast_const(),
                stream_create: std::mem::transmute::<*mut c_void, StreamCreate>(stream_create),
                set_dispatch_queue: std::mem::transmute::<*mut c_void, SetDispatchQueue>(
                    set_dispatch_queue,
                ),
                start: std::mem::transmute::<*mut c_void, StreamStart>(start),
                stop: std::mem::transmute::<*mut c_void, StreamOp>(stop),
                invalidate: std::mem::transmute::<*mut c_void, StreamOp>(invalidate),
                release: std::mem::transmute::<*mut c_void, StreamOp>(release),
            })
        }
    }
}

/// One watched path: as given, as the kernel reports it, and how deep.
#[derive(Clone)]
struct WatchedPath {
    given: PathBuf,
    canonical: PathBuf,
    recursive: bool,
}

/// What a stream's callbacks read; owned by the stream and freed by its
/// release callback.
struct CallbackContext {
    handler: Arc<Mutex<dyn EventHandler>>,
    paths: Vec<WatchedPath>,
}

struct Running {
    stream: StreamRef,
    queue: DispatchQueue,
}

/// A `notify::Watcher` over an FSEvents stream.
pub struct FsEventWatcher {
    symbols: Symbols,
    handler: Arc<Mutex<dyn EventHandler>>,
    paths: Vec<WatchedPath>,
    running: Option<Running>,
}

// SAFETY: the raw pointers are a stream and a queue that every method reaches
// through `&mut self`; FSEvents and dispatch objects may be used from any
// thread.
unsafe impl Send for FsEventWatcher {}

impl std::fmt::Debug for FsEventWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FsEventWatcher")
            .field(
                "paths",
                &self
                    .paths
                    .iter()
                    .map(|path| &path.given)
                    .collect::<Vec<_>>(),
            )
            .field("running", &self.running.is_some())
            .finish()
    }
}

impl FsEventWatcher {
    fn restart(&mut self) -> Result<()> {
        self.stop();
        if self.paths.is_empty() {
            return Ok(());
        }
        let symbols = &self.symbols;
        let mut strings = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            let raw = CString::new(path.given.as_os_str().as_bytes())
                .map_err(|_| Error::path_not_found().add_path(path.given.clone()))?;
            // SAFETY: a NUL-terminated path; the default allocator is NULL.
            let string = unsafe { (symbols.string_create)(std::ptr::null(), raw.as_ptr()) };
            if string.is_null() {
                for string in strings {
                    // SAFETY: each string was created above and is released once.
                    unsafe { (symbols.cf_release)(string) };
                }
                return Err(Error::path_not_found().add_path(path.given.clone()));
            }
            strings.push(string);
        }
        // SAFETY: `strings` holds `len` live CF strings; the array retains
        // them through the CF type callbacks, so they are released after.
        let array = unsafe {
            (symbols.array_create)(
                std::ptr::null(),
                strings.as_ptr(),
                strings.len() as isize,
                symbols.array_callbacks,
            )
        };
        for string in strings {
            // SAFETY: created above; the array holds its own reference.
            unsafe { (symbols.cf_release)(string) };
        }
        if array.is_null() {
            return Err(Error::generic("FSEvents path list could not be made"));
        }
        let info = Box::into_raw(Box::new(CallbackContext {
            handler: self.handler.clone(),
            paths: self.paths.clone(),
        }));
        let context = StreamContext {
            version: 0,
            info: info.cast(),
            retain: None,
            release: Some(release_context),
            copy_description: None,
        };
        // SAFETY: `context` and `array` are valid for the call; the stream
        // copies the context and retains the array.
        let stream = unsafe {
            (symbols.stream_create)(
                std::ptr::null(),
                callback,
                &context,
                array,
                SINCE_NOW,
                0.0,
                CREATE_FILE_EVENTS | CREATE_NO_DEFER,
            )
        };
        // SAFETY: created above; the stream holds its own reference.
        unsafe { (symbols.cf_release)(array) };
        if stream.is_null() {
            // SAFETY: no stream took the context, so it is still ours.
            drop(unsafe { Box::from_raw(info) });
            return Err(Error::generic("FSEvents stream could not be made"));
        }
        // SAFETY: a static label and the serial-queue attribute (NULL).
        let queue = unsafe { dispatch_queue_create(c"otter.fs-events".as_ptr(), std::ptr::null()) };
        // SAFETY: a live stream and queue.
        unsafe { (symbols.set_dispatch_queue)(stream, queue) };
        // SAFETY: a scheduled stream.
        if unsafe { (symbols.start)(stream) } == 0 {
            let mut retire = Retire {
                symbols,
                stream,
                stopped: false,
            };
            // SAFETY: the queue is live; `retire` outlives the synchronous call.
            unsafe { dispatch_sync_f(queue, (&raw mut retire).cast(), retire_on_queue) };
            // SAFETY: the queue is no longer used.
            unsafe { dispatch_release(queue) };
            return Err(Error::generic("FSEvents stream could not start"));
        }
        self.running = Some(Running { stream, queue });
        Ok(())
    }

    fn stop(&mut self) {
        let Some(Running { stream, queue }) = self.running.take() else {
            return;
        };
        let mut retire = Retire {
            symbols: &self.symbols,
            stream,
            stopped: true,
        };
        // SAFETY: the queue is live; running the retirement on it orders it
        // after every callback already queued, and `retire` outlives the
        // synchronous call.
        unsafe { dispatch_sync_f(queue, (&raw mut retire).cast(), retire_on_queue) };
        // SAFETY: the stream no longer targets the queue.
        unsafe { dispatch_release(queue) };
    }
}

/// A stream to stop, invalidate and release on its own queue.
struct Retire<'a> {
    symbols: &'a Symbols,
    stream: StreamRef,
    stopped: bool,
}

extern "C" fn retire_on_queue(context: *mut c_void) {
    // SAFETY: `dispatch_sync_f` passes the `Retire` its caller keeps alive.
    let retire = unsafe { &*context.cast::<Retire<'_>>() };
    // SAFETY: a live stream, retired once.
    unsafe {
        if retire.stopped {
            (retire.symbols.stop)(retire.stream);
        }
        (retire.symbols.invalidate)(retire.stream);
        (retire.symbols.release)(retire.stream);
    }
}

extern "C" fn release_context(info: *const c_void) {
    // SAFETY: the stream calls this once, when it is freed, with the context
    // `restart` boxed for it.
    drop(unsafe { Box::from_raw(info.cast_mut().cast::<CallbackContext>()) });
}

extern "C" fn callback(
    _stream: StreamRef,
    info: *mut c_void,
    count: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    // SAFETY: the stream passes the context it was made with, `count` C
    // strings (the stream does not use CF types) and `count` flag words.
    let (context, paths, flags) = unsafe {
        (
            &*info.cast::<CallbackContext>(),
            std::slice::from_raw_parts(paths.cast::<*const c_char>(), count),
            std::slice::from_raw_parts(flags, count),
        )
    };
    for (&raw, &flag) in paths.iter().zip(flags) {
        // SAFETY: each entry is a NUL-terminated path.
        let bytes = unsafe { CStr::from_ptr(raw) }.to_bytes();
        let path = PathBuf::from(std::ffi::OsStr::from_bytes(bytes));
        if !context.paths.iter().any(|watched| covers(watched, &path)) {
            continue;
        }
        let mut handler = context
            .handler
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for event in translate(flag) {
            handler.handle_event(Ok(event.add_path(path.clone())));
        }
    }
}

fn covers(watched: &WatchedPath, path: &Path) -> bool {
    path == watched.canonical
        || (path.starts_with(&watched.canonical)
            && (watched.recursive || path.parent() == Some(watched.canonical.as_path())))
}

/// The events one FSEvents flag word stands for.
fn translate(flags: u32) -> Vec<Event> {
    let has = |bit: u32| flags & bit != 0;
    let mut events = Vec::new();
    // The end of replayed history carries no change.
    if has(HISTORY_DONE) {
        return events;
    }
    if has(MUST_SCAN_SUBDIRS) {
        let event = Event::new(EventKind::Other).set_flag(Flag::Rescan);
        events.push(if has(USER_DROPPED) {
            event.set_info("rescan: user dropped")
        } else if has(KERNEL_DROPPED) {
            event.set_info("rescan: kernel dropped")
        } else {
            event
        });
    }
    if has(ROOT_CHANGED) {
        events.push(
            Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                .set_info("root changed"),
        );
    }
    if has(MOUNT) {
        events.push(Event::new(EventKind::Create(CreateKind::Other)).set_info("mount"));
    }
    if has(UNMOUNT) {
        events.push(Event::new(EventKind::Remove(RemoveKind::Other)).set_info("mount"));
    }
    let link_info = |event: Event| {
        if has(ITEM_IS_SYMLINK) {
            event.set_info("is: symlink")
        } else if has(ITEM_IS_HARDLINK) {
            event.set_info("is: hardlink")
        } else if has(ITEM_CLONED) {
            event.set_info("is: clone")
        } else {
            event
        }
    };
    if has(ITEM_CREATED) {
        events.push(if has(ITEM_IS_DIR) {
            Event::new(EventKind::Create(CreateKind::Folder))
        } else if has(ITEM_IS_FILE) {
            Event::new(EventKind::Create(CreateKind::File))
        } else {
            link_info(Event::new(EventKind::Create(CreateKind::Other)))
        });
    }
    if has(ITEM_REMOVED) {
        events.push(if has(ITEM_IS_DIR) {
            Event::new(EventKind::Remove(RemoveKind::Folder))
        } else if has(ITEM_IS_FILE) {
            Event::new(EventKind::Remove(RemoveKind::File))
        } else {
            link_info(Event::new(EventKind::Remove(RemoveKind::Other)))
        });
    }
    // FSEvents does not pair the two sides of a rename.
    if has(ITEM_RENAMED) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Name(
            RenameMode::Any,
        ))));
    }
    if has(ITEM_INODE_META_MOD) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        ))));
    }
    if has(ITEM_FINDER_INFO_MOD) {
        events.push(
            Event::new(EventKind::Modify(ModifyKind::Metadata(MetadataKind::Other)))
                .set_info("meta: finder info"),
        );
    }
    if has(ITEM_CHANGE_OWNER) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Ownership,
        ))));
    }
    if has(ITEM_XATTR_MOD) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Extended,
        ))));
    }
    if has(ITEM_MODIFIED) {
        events.push(Event::new(EventKind::Modify(ModifyKind::Data(
            DataChange::Content,
        ))));
    }
    if has(OWN_EVENT) {
        let pid = std::process::id();
        for event in &mut events {
            *event = std::mem::take(event).set_process_id(pid);
        }
    }
    events
}

impl Watcher for FsEventWatcher {
    fn new<F: EventHandler>(handler: F, _config: Config) -> Result<Self> {
        Ok(Self {
            symbols: Symbols::load()?,
            handler: Arc::new(Mutex::new(handler)),
            paths: Vec::new(),
            running: None,
        })
    }

    fn watch(&mut self, path: &Path, recursive_mode: RecursiveMode) -> Result<()> {
        if !path.exists() {
            return Err(Error::path_not_found().add_path(path.to_path_buf()));
        }
        let canonical = path.canonicalize().map_err(Error::io)?;
        self.paths.push(WatchedPath {
            given: path.to_path_buf(),
            canonical,
            recursive: matches!(recursive_mode, RecursiveMode::Recursive),
        });
        self.restart()
    }

    fn unwatch(&mut self, path: &Path) -> Result<()> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let before = self.paths.len();
        self.paths
            .retain(|watched| watched.given != path && watched.canonical != canonical);
        if self.paths.len() == before {
            return Err(Error::watch_not_found().add_path(path.to_path_buf()));
        }
        self.restart()
    }

    fn kind() -> WatcherKind {
        WatcherKind::Fsevent
    }
}

impl Drop for FsEventWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_created_file_is_a_file_creation() {
        let events = translate(ITEM_CREATED | ITEM_IS_FILE);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, EventKind::Create(CreateKind::File));
        assert!(translate(HISTORY_DONE | ITEM_CREATED).is_empty());
    }

    #[test]
    fn a_change_inside_the_watched_directory_reaches_the_handler() {
        let directory =
            std::env::temp_dir().join(format!("otter-fs-events-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut watcher = FsEventWatcher::new(sender, Config::default()).unwrap();
        watcher
            .watch(&directory, RecursiveMode::NonRecursive)
            .unwrap();
        let file = directory.join("touched.txt");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = false;
        while !seen && std::time::Instant::now() < deadline {
            std::fs::write(&file, b"x").unwrap();
            if let Ok(Ok(event)) = receiver.recv_timeout(std::time::Duration::from_millis(500)) {
                seen = event.paths.iter().any(|path| path.ends_with("touched.txt"));
            }
        }
        watcher.unwatch(&directory).unwrap();
        drop(watcher);
        std::fs::remove_dir_all(&directory).unwrap();
        assert!(seen, "no event for the written file");
    }
}
