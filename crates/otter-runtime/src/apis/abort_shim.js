/**
 * AbortController/AbortSignal implementation for Otter runtime.
 *
 * Implements the Web Cancellation API:
 * - EventTarget (base class)
 * - AbortSignal (extends EventTarget)
 * - AbortController (creates AbortSignal)
 * - DOMException (for AbortError/TimeoutError)
 */
(function() {
    'use strict';

    // DOMException implementation (minimal)
    if (typeof globalThis.DOMException === 'undefined') {
        class DOMException extends Error {
            constructor(message = '', name = 'Error') {
                super(message);
                this.name = name;
                this.code = DOMException._getCode(name);
            }

            static _getCode(name) {
                const codes = {
                    'IndexSizeError': 1,
                    'HierarchyRequestError': 3,
                    'WrongDocumentError': 4,
                    'InvalidCharacterError': 5,
                    'NoModificationAllowedError': 7,
                    'NotFoundError': 8,
                    'NotSupportedError': 9,
                    'InvalidStateError': 11,
                    'SyntaxError': 12,
                    'InvalidModificationError': 13,
                    'NamespaceError': 14,
                    'InvalidAccessError': 15,
                    'TypeMismatchError': 17,
                    'SecurityError': 18,
                    'NetworkError': 19,
                    'AbortError': 20,
                    'URLMismatchError': 21,
                    'QuotaExceededError': 22,
                    'TimeoutError': 23,
                    'InvalidNodeTypeError': 24,
                    'DataCloneError': 25,
                };
                return codes[name] || 0;
            }

            // Standard DOMException constants
            static get ABORT_ERR() { return 20; }
            static get TIMEOUT_ERR() { return 23; }
        }

        globalThis.DOMException = DOMException;
    }

    // Event class (minimal implementation)
    class Event {
        constructor(type, eventInitDict = {}) {
            this.type = type;
            this.target = null;
            this.currentTarget = null;
            this.eventPhase = 0;
            this.bubbles = eventInitDict.bubbles || false;
            this.cancelable = eventInitDict.cancelable || false;
            this.defaultPrevented = false;
            this.composed = eventInitDict.composed || false;
            this.timeStamp = performance.now();
            this.isTrusted = false;
        }

        preventDefault() {
            if (this.cancelable) {
                this.defaultPrevented = true;
            }
        }

        stopPropagation() {
            this._stopPropagation = true;
        }

        stopImmediatePropagation() {
            this._stopPropagation = true;
            this._stopImmediate = true;
        }
    }

    if (typeof globalThis.Event === 'undefined') {
        globalThis.Event = Event;
    }

    // EventTarget class
    class EventTarget {
        constructor() {
            this._listeners = new Map();
        }

        addEventListener(type, callback, options = {}) {
            if (callback === null || callback === undefined) {
                return;
            }

            if (typeof options === 'boolean') {
                options = { capture: options };
            }

            if (!this._listeners.has(type)) {
                this._listeners.set(type, []);
            }

            const listeners = this._listeners.get(type);

            // Check for duplicate
            const existing = listeners.find(l =>
                l.callback === callback && l.capture === (options.capture || false)
            );
            if (existing) {
                return;
            }

            const listener = {
                callback,
                once: options.once || false,
                capture: options.capture || false,
                passive: options.passive || false,
                signal: options.signal || null,
                removed: false,
            };

            listeners.push(listener);

            // Handle abort signal for automatic removal
            if (listener.signal) {
                if (listener.signal.aborted) {
                    // Already aborted, remove immediately
                    listener.removed = true;
                    const idx = listeners.indexOf(listener);
                    if (idx !== -1) listeners.splice(idx, 1);
                } else {
                    listener.signal.addEventListener('abort', () => {
                        this.removeEventListener(type, callback, options);
                    }, { once: true });
                }
            }
        }

        removeEventListener(type, callback, options = {}) {
            if (callback === null || callback === undefined) {
                return;
            }

            if (typeof options === 'boolean') {
                options = { capture: options };
            }

            const listeners = this._listeners.get(type);
            if (!listeners) return;

            const capture = options.capture || false;
            const idx = listeners.findIndex(l =>
                l.callback === callback && l.capture === capture
            );

            if (idx !== -1) {
                listeners[idx].removed = true;
                listeners.splice(idx, 1);
            }
        }

        dispatchEvent(event) {
            if (!(event instanceof Event)) {
                throw new TypeError("Failed to execute 'dispatchEvent': parameter 1 is not of type 'Event'.");
            }

            event.target = this;
            event.currentTarget = this;

            const listeners = this._listeners.get(event.type);
            if (!listeners) return true;

            // Copy array to handle modifications during iteration
            const listenersCopy = [...listeners];

            for (const listener of listenersCopy) {
                if (listener.removed) continue;
                if (event._stopImmediate) break;

                try {
                    if (typeof listener.callback === 'function') {
                        listener.callback.call(this, event);
                    } else if (typeof listener.callback.handleEvent === 'function') {
                        listener.callback.handleEvent(event);
                    }
                } catch (e) {
                    // Report error but continue
                    console.error('Error in event listener:', e);
                }

                if (listener.once) {
                    this.removeEventListener(event.type, listener.callback, { capture: listener.capture });
                }
            }

            return !event.defaultPrevented;
        }
    }

    // AbortSignal class
    class AbortSignal extends EventTarget {
        constructor() {
            super();
            this._aborted = false;
            this._reason = undefined;
            this.onabort = null;
        }

        get aborted() {
            return this._aborted;
        }

        get reason() {
            return this._reason;
        }

        throwIfAborted() {
            if (this._aborted) {
                throw this._reason;
            }
        }

        // Internal method - called by AbortController
        _abort(reason) {
            if (this._aborted) return;

            this._aborted = true;
            this._reason = reason !== undefined
                ? reason
                : new DOMException('This operation was aborted', 'AbortError');

            const event = new Event('abort');
            event.target = this;

            // Call onabort handler
            if (typeof this.onabort === 'function') {
                try {
                    this.onabort.call(this, event);
                } catch (e) {
                    console.error('Error in onabort handler:', e);
                }
            }

            // Dispatch to listeners
            this.dispatchEvent(event);
        }

        // Static factory methods
        static abort(reason) {
            const signal = new AbortSignal();
            signal._abort(reason);
            return signal;
        }

        static timeout(milliseconds) {
            if (typeof milliseconds !== 'number' || milliseconds < 0) {
                throw new TypeError('Timeout must be a non-negative number');
            }

            const signal = new AbortSignal();

            setTimeout(() => {
                signal._abort(new DOMException(
                    `Signal timed out after ${milliseconds}ms`,
                    'TimeoutError'
                ));
            }, milliseconds);

            return signal;
        }

        static any(signals) {
            if (!Array.isArray(signals)) {
                throw new TypeError('signals must be an array');
            }

            const signal = new AbortSignal();

            // Check if any signal is already aborted
            for (const s of signals) {
                if (!(s instanceof AbortSignal)) {
                    throw new TypeError('All signals must be AbortSignal instances');
                }
                if (s.aborted) {
                    signal._abort(s.reason);
                    return signal;
                }
            }

            // Listen for abort on all signals
            for (const s of signals) {
                s.addEventListener('abort', () => {
                    if (!signal.aborted) {
                        signal._abort(s.reason);
                    }
                }, { once: true });
            }

            return signal;
        }
    }

    // AbortController class
    class AbortController {
        constructor() {
            this._signal = new AbortSignal();
        }

        get signal() {
            return this._signal;
        }

        abort(reason) {
            this._signal._abort(reason);
        }
    }

    // Register globals
    globalThis.EventTarget = EventTarget;
    globalThis.AbortSignal = AbortSignal;
    globalThis.AbortController = AbortController;
})();
