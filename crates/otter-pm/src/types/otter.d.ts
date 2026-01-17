// Otter Runtime Type Definitions
// This file provides TypeScript type definitions for Otter's global APIs.

declare global {
    // ============================================================================
    // Console API
    // ============================================================================

    interface Console {
        /** Log a message to the console. */
        log(...args: any[]): void;
        /** Log an informational message. */
        info(...args: any[]): void;
        /** Log a warning message. */
        warn(...args: any[]): void;
        /** Log an error message. */
        error(...args: any[]): void;
        /** Log a debug message. */
        debug(...args: any[]): void;
    }

    const console: Console;

    // ============================================================================
    // Timer APIs
    // ============================================================================

    /**
     * Schedule a callback to run after a delay.
     * @param callback Function to call after the delay
     * @param ms Delay in milliseconds (default: 0)
     * @returns Timer ID for use with clearTimeout
     */
    function setTimeout(callback: () => void, ms?: number): number;

    /**
     * Schedule a callback to run repeatedly at an interval.
     * @param callback Function to call at each interval
     * @param ms Interval in milliseconds (default: 0)
     * @returns Timer ID for use with clearInterval
     */
    function setInterval(callback: () => void, ms?: number): number;

    /**
     * Cancel a timeout scheduled with setTimeout.
     * @param id Timer ID returned by setTimeout
     */
    function clearTimeout(id: number): void;

    /**
     * Cancel an interval scheduled with setInterval.
     * @param id Timer ID returned by setInterval
     */
    function clearInterval(id: number): void;

    /**
     * Queue a microtask to run after the current task completes.
     * @param callback Function to call as a microtask
     */
    function queueMicrotask(callback: () => void): void;

    // ============================================================================
    // Fetch API
    // ============================================================================

    interface RequestInit {
        method?: string;
        headers?: HeadersInit;
        body?: BodyInit | null;
        mode?: RequestMode;
        credentials?: RequestCredentials;
        cache?: RequestCache;
        redirect?: RequestRedirect;
        referrer?: string;
        referrerPolicy?: ReferrerPolicy;
        integrity?: string;
        keepalive?: boolean;
        signal?: AbortSignal | null;
    }

    interface Response {
        readonly ok: boolean;
        readonly status: number;
        readonly statusText: string;
        readonly headers: Headers;
        readonly url: string;
        readonly redirected: boolean;
        readonly type: ResponseType;

        clone(): Response;
        arrayBuffer(): Promise<ArrayBuffer>;
        blob(): Promise<Blob>;
        formData(): Promise<FormData>;
        json(): Promise<any>;
        text(): Promise<string>;
    }

    interface Headers {
        append(name: string, value: string): void;
        delete(name: string): void;
        get(name: string): string | null;
        has(name: string): boolean;
        set(name: string, value: string): void;
        forEach(callback: (value: string, name: string, parent: Headers) => void): void;
    }

    /**
     * Fetch a resource from the network.
     * @param input URL or Request object
     * @param init Optional request configuration
     * @returns Promise resolving to the Response
     */
    function fetch(input: string | Request, init?: RequestInit): Promise<Response>;

    // ============================================================================
    // Encoding APIs
    // ============================================================================

    /**
     * Encode a string to UTF-8 bytes.
     */
    class TextEncoder {
        readonly encoding: string;
        encode(input?: string): Uint8Array;
        encodeInto(source: string, destination: Uint8Array): { read: number; written: number };
    }

    interface TextDecodeOptions {
        stream?: boolean;
    }

    interface TextDecoderOptions {
        fatal?: boolean;
        ignoreBOM?: boolean;
    }

    /**
     * Decode bytes to a string.
     */
    class TextDecoder {
        readonly encoding: string;
        readonly fatal: boolean;
        readonly ignoreBOM: boolean;
        constructor(label?: string, options?: TextDecoderOptions);
        decode(input?: BufferSource, options?: TextDecodeOptions): string;
    }

    // ============================================================================
    // URL APIs
    // ============================================================================

    class URL {
        constructor(url: string, base?: string | URL);
        hash: string;
        host: string;
        hostname: string;
        href: string;
        readonly origin: string;
        password: string;
        pathname: string;
        port: string;
        protocol: string;
        search: string;
        readonly searchParams: URLSearchParams;
        username: string;
        toJSON(): string;
        toString(): string;
    }

    class URLSearchParams {
        constructor(init?: string | URLSearchParams | Record<string, string> | [string, string][]);
        append(name: string, value: string): void;
        delete(name: string): void;
        get(name: string): string | null;
        getAll(name: string): string[];
        has(name: string): boolean;
        set(name: string, value: string): void;
        sort(): void;
        toString(): string;
        forEach(callback: (value: string, name: string, parent: URLSearchParams) => void): void;
        entries(): IterableIterator<[string, string]>;
        keys(): IterableIterator<string>;
        values(): IterableIterator<string>;
        [Symbol.iterator](): IterableIterator<[string, string]>;
    }

    // ============================================================================
    // Event API
    // ============================================================================

    interface EventListenerOptions {
        capture?: boolean;
    }

    interface AddEventListenerOptions extends EventListenerOptions {
        once?: boolean;
        passive?: boolean;
        signal?: AbortSignal;
    }

    interface EventListener {
        (evt: Event): void;
    }

    interface EventListenerObject {
        handleEvent(object: Event): void;
    }

    type EventListenerOrEventListenerObject = EventListener | EventListenerObject;

    /**
     * EventTarget is the base interface for objects that can receive events.
     */
    interface EventTarget {
        /**
         * Appends an event listener for events whose type attribute value is type.
         */
        addEventListener(
            type: string,
            callback: EventListenerOrEventListenerObject | null,
            options?: AddEventListenerOptions | boolean
        ): void;

        /**
         * Removes the event listener in target's event listener list with the same type, callback, and options.
         */
        removeEventListener(
            type: string,
            callback: EventListenerOrEventListenerObject | null,
            options?: EventListenerOptions | boolean
        ): void;

        /**
         * Dispatches a synthetic event to target.
         */
        dispatchEvent(event: Event): boolean;
    }

    /**
     * EventTarget constructor for creating custom event targets.
     */
    const EventTarget: {
        prototype: EventTarget;
        new(): EventTarget;
    };

    /**
     * Event interface represents an event which takes place in the runtime.
     */
    interface Event {
        readonly type: string;
        readonly target: EventTarget | null;
        readonly currentTarget: EventTarget | null;
        readonly eventPhase: number;
        readonly bubbles: boolean;
        readonly cancelable: boolean;
        readonly defaultPrevented: boolean;
        readonly composed: boolean;
        readonly timeStamp: number;
        readonly isTrusted: boolean;

        preventDefault(): void;
        stopPropagation(): void;
        stopImmediatePropagation(): void;
    }

    /**
     * Event constructor.
     */
    const Event: {
        prototype: Event;
        new(type: string, eventInitDict?: EventInit): Event;
    };

    interface EventInit {
        bubbles?: boolean;
        cancelable?: boolean;
        composed?: boolean;
    }

    /**
     * DOMException represents an abnormal event during DOM operations.
     */
    interface DOMException extends Error {
        readonly name: string;
        readonly message: string;
        readonly code: number;
    }

    const DOMException: {
        prototype: DOMException;
        new(message?: string, name?: string): DOMException;
        readonly ABORT_ERR: number;
        readonly TIMEOUT_ERR: number;
    };

    // ============================================================================
    // Abort API
    // ============================================================================

    /**
     * AbortController is used to abort one or more Web requests.
     */
    class AbortController {
        /** Returns the AbortSignal object associated with this object. */
        readonly signal: AbortSignal;

        /**
         * Invoking this method will set this object's AbortSignal's aborted flag
         * and signal to any observers that the associated activity is to be aborted.
         */
        abort(reason?: any): void;
    }

    /**
     * AbortSignal represents a signal object that allows you to communicate
     * with a request and abort it if required.
     */
    interface AbortSignal extends EventTarget {
        /** Returns true if this AbortSignal's AbortController has signaled to abort. */
        readonly aborted: boolean;

        /** Returns the abort reason, if any. */
        readonly reason: any;

        /** Event handler called when an abort event is raised. */
        onabort: ((this: AbortSignal, ev: Event) => any) | null;

        /** Throws this AbortSignal's abort reason if aborted. */
        throwIfAborted(): void;
    }

    const AbortSignal: {
        prototype: AbortSignal;
        new(): AbortSignal;

        /** Returns an AbortSignal instance that is already set as aborted. */
        abort(reason?: any): AbortSignal;

        /** Returns an AbortSignal instance that will automatically abort after a specified time. */
        timeout(milliseconds: number): AbortSignal;

        /** Returns an AbortSignal that aborts when any of the given signals aborts. */
        any(signals: AbortSignal[]): AbortSignal;
    };

    // ============================================================================
    // Structured Clone
    // ============================================================================

    /**
     * Create a deep clone of a value using the structured clone algorithm.
     */
    function structuredClone<T>(value: T, options?: StructuredSerializeOptions): T;

    interface StructuredSerializeOptions {
        transfer?: Transferable[];
    }

    // ============================================================================
    // Performance API
    // ============================================================================

    interface Performance {
        now(): number;
        timeOrigin: number;
    }

    const performance: Performance;

    // ============================================================================
    // Crypto API (basic)
    // ============================================================================

    interface Crypto {
        getRandomValues<T extends ArrayBufferView>(array: T): T;
        randomUUID(): string;
    }

    const crypto: Crypto;

    // ============================================================================
    // CommonJS Support
    // ============================================================================

    /**
     * Require a CommonJS module.
     * @param id Module specifier (path or package name)
     * @returns The module's exports
     */
    function require(id: string): any;

    /**
     * The require function interface with additional properties.
     */
    interface NodeRequire {
        (id: string): any;

        /**
         * Resolve a module path to its absolute path.
         */
        resolve(id: string): string;

        /**
         * Module cache - loaded modules are cached here.
         */
        cache: Record<string, NodeModule>;

        /**
         * The main module (entry point).
         */
        main: NodeModule | undefined;
    }

    /**
     * The module object available in CommonJS modules.
     */
    interface NodeModule {
        /**
         * The module's exports object.
         */
        exports: any;

        /**
         * The require function for this module.
         */
        require: NodeRequire;

        /**
         * The module's unique identifier.
         */
        id: string;

        /**
         * The absolute path to the module file.
         */
        filename: string;

        /**
         * Whether the module has finished loading.
         */
        loaded: boolean;

        /**
         * The module that first required this one.
         */
        parent: NodeModule | null;

        /**
         * Modules that have been required by this module.
         */
        children: NodeModule[];

        /**
         * The search paths for modules.
         */
        paths: string[];
    }

    /**
     * The module object - available in CommonJS modules.
     */
    var module: NodeModule;

    /**
     * Alias to module.exports - available in CommonJS modules.
     */
    var exports: any;

    /**
     * The directory name of the current module - available in CommonJS modules.
     */
    var __dirname: string;

    /**
     * The file name of the current module - available in CommonJS modules.
     */
    var __filename: string;
}

export {};
