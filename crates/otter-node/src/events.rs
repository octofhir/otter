//! Node.js `events` module implementation (EventEmitter).
//!
//! Provides a Node.js-compatible EventEmitter class for event-driven programming.
//!
//! # Example
//!
//! ```javascript
//! const { EventEmitter } = require('events');
//!
//! const emitter = new EventEmitter();
//! emitter.on('data', (chunk) => console.log(chunk));
//! emitter.emit('data', 'Hello!');
//! ```

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Default maximum number of listeners per event.
pub const DEFAULT_MAX_LISTENERS: usize = 10;

/// Unique listener ID for tracking callbacks.
static NEXT_LISTENER_ID: AtomicU64 = AtomicU64::new(1);

/// Generate a unique listener ID.
fn next_listener_id() -> u64 {
    NEXT_LISTENER_ID.fetch_add(1, Ordering::SeqCst)
}

/// Represents a registered event listener.
#[derive(Debug, Clone)]
pub struct Listener {
    /// Unique identifier for this listener.
    pub id: u64,

    /// Whether this is a one-time listener (added via `once`).
    pub once: bool,

    /// Whether this listener should be called before others (prepend).
    pub prepend: bool,
}

impl Listener {
    /// Create a new regular listener.
    pub fn new() -> Self {
        Self {
            id: next_listener_id(),
            once: false,
            prepend: false,
        }
    }

    /// Create a new one-time listener.
    pub fn once() -> Self {
        Self {
            id: next_listener_id(),
            once: true,
            prepend: false,
        }
    }

    /// Create a new prepended listener.
    pub fn prepend() -> Self {
        Self {
            id: next_listener_id(),
            once: false,
            prepend: true,
        }
    }

    /// Create a new prepended one-time listener.
    pub fn prepend_once() -> Self {
        Self {
            id: next_listener_id(),
            once: true,
            prepend: true,
        }
    }
}

impl Default for Listener {
    fn default() -> Self {
        Self::new()
    }
}

/// Event emitter state managed in Rust.
///
/// This tracks listeners by event name and ID. The actual callback functions
/// are stored in JavaScript - this just manages the metadata.
#[derive(Debug, Default)]
pub struct EventEmitterState {
    /// Listeners indexed by event name, storing (listener_id, once_flag).
    listeners: HashMap<String, Vec<Listener>>,

    /// Maximum listeners per event (0 = unlimited).
    max_listeners: usize,

    /// Total number of listeners across all events.
    listener_count: usize,
}

impl EventEmitterState {
    /// Create a new event emitter state with default max listeners.
    pub fn new() -> Self {
        Self {
            listeners: HashMap::new(),
            max_listeners: DEFAULT_MAX_LISTENERS,
            listener_count: 0,
        }
    }

    /// Set the maximum number of listeners per event.
    pub fn set_max_listeners(&mut self, n: usize) {
        self.max_listeners = n;
    }

    /// Get the maximum number of listeners per event.
    pub fn get_max_listeners(&self) -> usize {
        self.max_listeners
    }

    /// Add a listener for an event. Returns the listener ID and whether
    /// a max listeners warning should be emitted.
    pub fn add_listener(&mut self, event: &str, listener: Listener) -> (u64, bool) {
        let id = listener.id;
        let prepend = listener.prepend;

        let listeners = self.listeners.entry(event.to_string()).or_default();

        if prepend {
            listeners.insert(0, listener);
        } else {
            listeners.push(listener);
        }

        self.listener_count += 1;

        // Check if we should warn about max listeners
        let should_warn = self.max_listeners > 0 && listeners.len() > self.max_listeners;

        (id, should_warn)
    }

    /// Remove a listener by ID. Returns true if the listener was found and removed.
    pub fn remove_listener(&mut self, event: &str, listener_id: u64) -> bool {
        if let Some(listeners) = self.listeners.get_mut(event) {
            if let Some(pos) = listeners.iter().position(|l| l.id == listener_id) {
                listeners.remove(pos);
                self.listener_count -= 1;
                return true;
            }
        }
        false
    }

    /// Remove all listeners for an event, or all events if event is None.
    /// Returns the list of removed listener IDs.
    pub fn remove_all_listeners(&mut self, event: Option<&str>) -> Vec<u64> {
        let mut removed = Vec::new();

        match event {
            Some(event_name) => {
                if let Some(listeners) = self.listeners.remove(event_name) {
                    for listener in listeners {
                        removed.push(listener.id);
                        self.listener_count -= 1;
                    }
                }
            }
            None => {
                for (_, listeners) in self.listeners.drain() {
                    for listener in listeners {
                        removed.push(listener.id);
                    }
                }
                self.listener_count = 0;
            }
        }

        removed
    }

    /// Get listener IDs for an event in order they should be called.
    pub fn listeners(&self, event: &str) -> Vec<u64> {
        self.listeners
            .get(event)
            .map(|l| l.iter().map(|listener| listener.id).collect())
            .unwrap_or_default()
    }

    /// Get the number of listeners for an event.
    pub fn listener_count(&self, event: &str) -> usize {
        self.listeners.get(event).map(|l| l.len()).unwrap_or(0)
    }

    /// Get all event names that have listeners.
    pub fn event_names(&self) -> Vec<String> {
        self.listeners
            .iter()
            .filter(|(_, v)| !v.is_empty())
            .map(|(k, _)| k.clone())
            .collect()
    }

    /// Mark once listeners as needing removal after emit.
    /// Returns the IDs of listeners that should be removed.
    pub fn get_once_listeners(&self, event: &str) -> Vec<u64> {
        self.listeners
            .get(event)
            .map(|l| {
                l.iter()
                    .filter(|listener| listener.once)
                    .map(|listener| listener.id)
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Thread-safe wrapper around EventEmitterState.
#[derive(Debug, Clone, Default)]
pub struct EventEmitter {
    state: Arc<Mutex<EventEmitterState>>,
}

impl EventEmitter {
    /// Create a new EventEmitter.
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(EventEmitterState::new())),
        }
    }

    /// Set max listeners.
    pub fn set_max_listeners(&self, n: usize) {
        self.state.lock().set_max_listeners(n);
    }

    /// Get max listeners.
    pub fn get_max_listeners(&self) -> usize {
        self.state.lock().get_max_listeners()
    }

    /// Add a listener.
    pub fn add_listener(&self, event: &str, listener: Listener) -> (u64, bool) {
        self.state.lock().add_listener(event, listener)
    }

    /// Remove a listener.
    pub fn remove_listener(&self, event: &str, listener_id: u64) -> bool {
        self.state.lock().remove_listener(event, listener_id)
    }

    /// Remove all listeners.
    pub fn remove_all_listeners(&self, event: Option<&str>) -> Vec<u64> {
        self.state.lock().remove_all_listeners(event)
    }

    /// Get listeners for an event.
    pub fn listeners(&self, event: &str) -> Vec<u64> {
        self.state.lock().listeners(event)
    }

    /// Get listener count for an event.
    pub fn listener_count(&self, event: &str) -> usize {
        self.state.lock().listener_count(event)
    }

    /// Get all event names.
    pub fn event_names(&self) -> Vec<String> {
        self.state.lock().event_names()
    }

    /// Get once listeners that should be removed after emit.
    pub fn get_once_listeners(&self, event: &str) -> Vec<u64> {
        self.state.lock().get_once_listeners(event)
    }
}

/// Generate JavaScript code for EventEmitter class.
///
/// This creates a full-featured EventEmitter class in JavaScript that
/// matches Node.js behavior.
pub fn event_emitter_js() -> &'static str {
    r#"
(function() {
    const DEFAULT_MAX_LISTENERS = 10;

    class EventEmitter {
        constructor() {
            this._events = new Map();
            this._maxListeners = DEFAULT_MAX_LISTENERS;
        }

        // Add listener
        addListener(event, listener) {
            return this.on(event, listener);
        }

        // Add listener (alias)
        on(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            const listeners = this._events.get(event);
            listeners.push({ fn: listener, once: false });

            // Warn if max listeners exceeded
            if (this._maxListeners > 0 && listeners.length > this._maxListeners) {
                console.warn(
                    `MaxListenersExceededWarning: Possible EventEmitter memory leak detected. ` +
                    `${listeners.length} ${event} listeners added. Use emitter.setMaxListeners() to increase limit`
                );
            }

            // Emit 'newListener' event
            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        // Add one-time listener
        once(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).push({ fn: listener, once: true });

            // Emit 'newListener' event
            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        // Add listener at beginning
        prependListener(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).unshift({ fn: listener, once: false });

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        // Add one-time listener at beginning
        prependOnceListener(event, listener) {
            if (typeof listener !== 'function') {
                throw new TypeError('The "listener" argument must be of type Function');
            }

            if (!this._events.has(event)) {
                this._events.set(event, []);
            }

            this._events.get(event).unshift({ fn: listener, once: true });

            if (event !== 'newListener' && this._events.has('newListener')) {
                this.emit('newListener', event, listener);
            }

            return this;
        }

        // Remove listener
        removeListener(event, listener) {
            return this.off(event, listener);
        }

        // Remove listener (alias)
        off(event, listener) {
            if (!this._events.has(event)) {
                return this;
            }

            const listeners = this._events.get(event);
            const index = listeners.findIndex(l => l.fn === listener);

            if (index !== -1) {
                listeners.splice(index, 1);

                // Emit 'removeListener' event
                if (event !== 'removeListener' && this._events.has('removeListener')) {
                    this.emit('removeListener', event, listener);
                }
            }

            return this;
        }

        // Remove all listeners for event (or all events)
        removeAllListeners(event) {
            if (event === undefined) {
                // Remove all listeners for all events
                const events = [...this._events.keys()];
                for (const e of events) {
                    if (e !== 'removeListener') {
                        this.removeAllListeners(e);
                    }
                }
                this._events.delete('removeListener');
            } else if (this._events.has(event)) {
                // Emit 'removeListener' for each listener
                if (event !== 'removeListener' && this._events.has('removeListener')) {
                    const listeners = this._events.get(event);
                    for (const l of listeners) {
                        this.emit('removeListener', event, l.fn);
                    }
                }
                this._events.delete(event);
            }

            return this;
        }

        // Emit event
        emit(event, ...args) {
            if (!this._events.has(event)) {
                // Special handling for 'error' event
                if (event === 'error') {
                    const err = args[0];
                    if (err instanceof Error) {
                        throw err;
                    }
                    throw new Error('Unhandled error: ' + err);
                }
                return false;
            }

            const listeners = this._events.get(event);
            if (listeners.length === 0) {
                if (event === 'error') {
                    const err = args[0];
                    if (err instanceof Error) {
                        throw err;
                    }
                    throw new Error('Unhandled error: ' + err);
                }
                return false;
            }

            // Copy array to allow modifications during iteration
            const toCall = [...listeners];

            // Remove once listeners
            this._events.set(event, listeners.filter(l => !l.once));

            // Call listeners
            for (const listener of toCall) {
                try {
                    listener.fn.apply(this, args);
                } catch (err) {
                    // If error occurs and we're not emitting 'error', emit it
                    if (event !== 'error') {
                        this.emit('error', err);
                    } else {
                        throw err;
                    }
                }
            }

            return true;
        }

        // Get listeners for event
        listeners(event) {
            if (!this._events.has(event)) {
                return [];
            }
            return this._events.get(event).map(l => l.fn);
        }

        // Get raw listeners (includes wrapper info)
        rawListeners(event) {
            if (!this._events.has(event)) {
                return [];
            }
            return [...this._events.get(event)];
        }

        // Get listener count
        listenerCount(event) {
            if (!this._events.has(event)) {
                return 0;
            }
            return this._events.get(event).length;
        }

        // Get all event names
        eventNames() {
            return [...this._events.keys()].filter(e => this._events.get(e).length > 0);
        }

        // Set max listeners
        setMaxListeners(n) {
            if (typeof n !== 'number' || n < 0 || Number.isNaN(n)) {
                throw new RangeError('The "n" argument must be a non-negative number');
            }
            this._maxListeners = n;
            return this;
        }

        // Get max listeners
        getMaxListeners() {
            return this._maxListeners;
        }

        // Static: Get default max listeners
        static get defaultMaxListeners() {
            return DEFAULT_MAX_LISTENERS;
        }

        // Static: Set default max listeners
        static set defaultMaxListeners(n) {
            // Note: This would need shared state, simplified here
            console.warn('EventEmitter.defaultMaxListeners is read-only in Otter');
        }

        // Static: Create event listener that resolves on first emit
        static once(emitter, event, options = {}) {
            return new Promise((resolve, reject) => {
                const signal = options?.signal;

                if (signal?.aborted) {
                    reject(new Error('The operation was aborted'));
                    return;
                }

                const listener = (...args) => {
                    if (errorListener) {
                        emitter.off('error', errorListener);
                    }
                    resolve(args);
                };

                let errorListener;
                if (event !== 'error') {
                    errorListener = (err) => {
                        emitter.off(event, listener);
                        reject(err);
                    };
                    emitter.once('error', errorListener);
                }

                emitter.once(event, listener);

                if (signal) {
                    signal.addEventListener('abort', () => {
                        emitter.off(event, listener);
                        if (errorListener) {
                            emitter.off('error', errorListener);
                        }
                        reject(new Error('The operation was aborted'));
                    }, { once: true });
                }
            });
        }

        // Static: Create async iterator for events
        static on(emitter, event, options = {}) {
            const signal = options?.signal;
            const unconsumedEvents = [];
            const unconsumedPromises = [];
            let finished = false;
            let error = null;

            const eventHandler = (...args) => {
                if (unconsumedPromises.length > 0) {
                    unconsumedPromises.shift().resolve({ value: args, done: false });
                } else {
                    unconsumedEvents.push(args);
                }
            };

            const errorHandler = (err) => {
                error = err;
                if (unconsumedPromises.length > 0) {
                    unconsumedPromises.shift().reject(err);
                }
            };

            emitter.on(event, eventHandler);
            if (event !== 'error') {
                emitter.on('error', errorHandler);
            }

            return {
                [Symbol.asyncIterator]() {
                    return this;
                },
                next() {
                    if (unconsumedEvents.length > 0) {
                        return Promise.resolve({ value: unconsumedEvents.shift(), done: false });
                    }

                    if (finished) {
                        return Promise.resolve({ done: true });
                    }

                    if (error) {
                        return Promise.reject(error);
                    }

                    return new Promise((resolve, reject) => {
                        unconsumedPromises.push({ resolve, reject });
                    });
                },
                return() {
                    finished = true;
                    emitter.off(event, eventHandler);
                    emitter.off('error', errorHandler);

                    for (const promise of unconsumedPromises) {
                        promise.resolve({ done: true });
                    }

                    return Promise.resolve({ done: true });
                },
                throw(err) {
                    error = err;
                    emitter.off(event, eventHandler);
                    emitter.off('error', errorHandler);
                    return Promise.reject(err);
                }
            };
        }

        // Static: Get listener count from any emitter
        static listenerCount(emitter, event) {
            if (typeof emitter.listenerCount === 'function') {
                return emitter.listenerCount(event);
            }
            return 0;
        }
    }

    // Export to globalThis for module system
    globalThis.__EventEmitter = EventEmitter;

    // Create events module
    const eventsModule = {
        EventEmitter,
        once: EventEmitter.once,
        on: EventEmitter.on,
        listenerCount: EventEmitter.listenerCount,
        default: EventEmitter,
    };

    // Register with module system if available
    if (globalThis.__registerModule) {
        globalThis.__registerModule('events', eventsModule);
        globalThis.__registerModule('node:events', eventsModule);
    }
})();
"#
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_listener_creation() {
        let listener = Listener::new();
        assert!(!listener.once);
        assert!(!listener.prepend);

        let once_listener = Listener::once();
        assert!(once_listener.once);
        assert!(!once_listener.prepend);

        let prepend_listener = Listener::prepend();
        assert!(!prepend_listener.once);
        assert!(prepend_listener.prepend);
    }

    #[test]
    fn test_add_and_remove_listeners() {
        let emitter = EventEmitter::new();

        let (id1, _) = emitter.add_listener("test", Listener::new());
        let (id2, _) = emitter.add_listener("test", Listener::new());

        assert_eq!(emitter.listener_count("test"), 2);
        assert_eq!(emitter.listeners("test"), vec![id1, id2]);

        emitter.remove_listener("test", id1);
        assert_eq!(emitter.listener_count("test"), 1);
        assert_eq!(emitter.listeners("test"), vec![id2]);
    }

    #[test]
    fn test_prepend_listener() {
        let emitter = EventEmitter::new();

        let (id1, _) = emitter.add_listener("test", Listener::new());
        let (id2, _) = emitter.add_listener("test", Listener::prepend());

        // Prepended listener should be first
        assert_eq!(emitter.listeners("test"), vec![id2, id1]);
    }

    #[test]
    fn test_max_listeners_warning() {
        let emitter = EventEmitter::new();
        emitter.set_max_listeners(2);

        let (_, warn1) = emitter.add_listener("test", Listener::new());
        let (_, warn2) = emitter.add_listener("test", Listener::new());
        let (_, warn3) = emitter.add_listener("test", Listener::new());

        assert!(!warn1);
        assert!(!warn2);
        assert!(warn3); // Should warn on third listener
    }

    #[test]
    fn test_remove_all_listeners() {
        let emitter = EventEmitter::new();

        emitter.add_listener("event1", Listener::new());
        emitter.add_listener("event1", Listener::new());
        emitter.add_listener("event2", Listener::new());

        // Remove all listeners for event1
        let removed = emitter.remove_all_listeners(Some("event1"));
        assert_eq!(removed.len(), 2);
        assert_eq!(emitter.listener_count("event1"), 0);
        assert_eq!(emitter.listener_count("event2"), 1);

        // Remove all remaining listeners
        let removed = emitter.remove_all_listeners(None);
        assert_eq!(removed.len(), 1);
        assert_eq!(emitter.event_names().len(), 0);
    }

    #[test]
    fn test_event_names() {
        let emitter = EventEmitter::new();

        emitter.add_listener("data", Listener::new());
        emitter.add_listener("error", Listener::new());
        emitter.add_listener("end", Listener::new());

        let names = emitter.event_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"data".to_string()));
        assert!(names.contains(&"error".to_string()));
        assert!(names.contains(&"end".to_string()));
    }

    #[test]
    fn test_once_listeners() {
        let emitter = EventEmitter::new();

        let (id1, _) = emitter.add_listener("test", Listener::new());
        let (id2, _) = emitter.add_listener("test", Listener::once());
        let (id3, _) = emitter.add_listener("test", Listener::new());

        let once_ids = emitter.get_once_listeners("test");
        assert_eq!(once_ids, vec![id2]);

        // All listeners should still be in the list
        assert_eq!(emitter.listeners("test"), vec![id1, id2, id3]);
    }

    #[test]
    fn test_js_code_generation() {
        let js = event_emitter_js();
        assert!(js.contains("class EventEmitter"));
        assert!(js.contains("addListener"));
        assert!(js.contains("removeListener"));
        assert!(js.contains("emit"));
        assert!(js.contains("once"));
    }
}
