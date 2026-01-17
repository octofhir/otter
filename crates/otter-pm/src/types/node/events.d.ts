/**
 * The `node:events` module provides an EventEmitter class that is the foundation
 * of Node.js's event-driven architecture.
 * @module node:events
 */
declare module "node:events" {
    /**
     * Event listener function type.
     */
    export type Listener = (...args: any[]) => void;

    /**
     * Options for EventEmitter.once() static method.
     */
    export interface EventEmitterOnceOptions {
        /** Optional AbortSignal to cancel waiting for the event */
        signal?: AbortSignal;
    }

    /**
     * Options for EventEmitter.on() static method.
     */
    export interface EventEmitterOnOptions {
        /** Optional AbortSignal to cancel the async iterator */
        signal?: AbortSignal;
    }

    /**
     * The EventEmitter class is defined and exposed by the events module.
     * All EventEmitters emit the event 'newListener' when new listeners are
     * added and 'removeListener' when existing listeners are removed.
     *
     * @example
     * ```ts
     * import { EventEmitter } from 'node:events';
     *
     * const emitter = new EventEmitter();
     *
     * emitter.on('data', (chunk) => {
     *   console.log('received:', chunk);
     * });
     *
     * emitter.emit('data', 'Hello, World!');
     * ```
     */
    export class EventEmitter {
        /**
         * Creates a new EventEmitter instance.
         */
        constructor();

        /**
         * Default maximum number of listeners per event.
         */
        static defaultMaxListeners: number;

        /**
         * Returns a copy of the array of listeners for the event named eventName,
         * including any wrappers (such as those created by .once()).
         * @param emitter The emitter to query
         * @param event The event name
         */
        static listenerCount(emitter: EventEmitter, event: string | symbol): number;

        /**
         * Creates a Promise that is fulfilled when the EventEmitter emits the given
         * event or that is rejected if the EventEmitter emits 'error'.
         * @param emitter The emitter to wait on
         * @param event The event name to wait for
         * @param options Optional settings
         * @example
         * ```ts
         * const [data] = await EventEmitter.once(emitter, 'data');
         * ```
         */
        static once(
            emitter: EventEmitter,
            event: string | symbol,
            options?: EventEmitterOnceOptions
        ): Promise<any[]>;

        /**
         * Returns an AsyncIterator that iterates eventName events.
         * @param emitter The emitter to iterate
         * @param event The event name to iterate
         * @param options Optional settings
         * @example
         * ```ts
         * for await (const [data] of EventEmitter.on(emitter, 'data')) {
         *   console.log(data);
         * }
         * ```
         */
        static on(
            emitter: EventEmitter,
            event: string | symbol,
            options?: EventEmitterOnOptions
        ): AsyncIterableIterator<any[]>;

        /**
         * Alias for emitter.on(eventName, listener).
         * @param event The event name
         * @param listener The callback function
         */
        addListener(event: string | symbol, listener: Listener): this;

        /**
         * Adds the listener function to the end of the listeners array for the
         * event named eventName.
         * @param event The event name
         * @param listener The callback function
         * @example
         * ```ts
         * emitter.on('data', (chunk) => console.log(chunk));
         * ```
         */
        on(event: string | symbol, listener: Listener): this;

        /**
         * Adds a one-time listener function for the event named eventName.
         * The listener is invoked only the next time eventName is triggered,
         * after which it is removed.
         * @param event The event name
         * @param listener The callback function
         * @example
         * ```ts
         * emitter.once('connect', () => console.log('Connected!'));
         * ```
         */
        once(event: string | symbol, listener: Listener): this;

        /**
         * Adds the listener function to the beginning of the listeners array.
         * @param event The event name
         * @param listener The callback function
         */
        prependListener(event: string | symbol, listener: Listener): this;

        /**
         * Adds a one-time listener to the beginning of the listeners array.
         * @param event The event name
         * @param listener The callback function
         */
        prependOnceListener(event: string | symbol, listener: Listener): this;

        /**
         * Alias for emitter.removeListener().
         * @param event The event name
         * @param listener The callback function to remove
         */
        off(event: string | symbol, listener: Listener): this;

        /**
         * Removes the specified listener from the listener array for the event.
         * @param event The event name
         * @param listener The callback function to remove
         */
        removeListener(event: string | symbol, listener: Listener): this;

        /**
         * Removes all listeners, or those of the specified eventName.
         * @param event Optional event name to remove listeners for
         */
        removeAllListeners(event?: string | symbol): this;

        /**
         * Synchronously calls each of the listeners registered for the event
         * named eventName, in the order they were registered, passing the
         * supplied arguments to each.
         * @param event The event name
         * @param args Arguments to pass to listeners
         * @returns true if the event had listeners, false otherwise
         * @example
         * ```ts
         * emitter.emit('data', 'chunk1', 'chunk2');
         * ```
         */
        emit(event: string | symbol, ...args: any[]): boolean;

        /**
         * Returns a copy of the array of listeners for the event named eventName.
         * @param event The event name
         */
        listeners(event: string | symbol): Listener[];

        /**
         * Returns a copy of the array of listeners for the event named eventName,
         * including any wrappers (such as those created by .once()).
         * @param event The event name
         */
        rawListeners(event: string | symbol): Listener[];

        /**
         * Returns the number of listeners listening to the event named eventName.
         * @param event The event name
         */
        listenerCount(event: string | symbol): number;

        /**
         * Returns an array listing the events for which the emitter has
         * registered listeners.
         */
        eventNames(): (string | symbol)[];

        /**
         * By default EventEmitters will print a warning if more than 10 listeners
         * are added for a particular event. This is a useful default which helps
         * finding memory leaks. This method allows that limit to be modified.
         * @param n The maximum number of listeners
         */
        setMaxListeners(n: number): this;

        /**
         * Returns the current max listener value for the EventEmitter.
         */
        getMaxListeners(): number;
    }

    /**
     * Creates a Promise that is fulfilled when the EventEmitter emits the given
     * event or that is rejected if the EventEmitter emits 'error'.
     */
    export function once(
        emitter: EventEmitter,
        event: string | symbol,
        options?: EventEmitterOnceOptions
    ): Promise<any[]>;

    /**
     * Returns an AsyncIterator that iterates eventName events.
     */
    export function on(
        emitter: EventEmitter,
        event: string | symbol,
        options?: EventEmitterOnOptions
    ): AsyncIterableIterator<any[]>;

    /**
     * Returns the number of listeners listening to the event named eventName.
     */
    export function listenerCount(emitter: EventEmitter, event: string | symbol): number;

    export default EventEmitter;
}

// Also support the 'events' module (without node: prefix)
declare module "events" {
    export * from "node:events";
    export { default } from "node:events";
}
