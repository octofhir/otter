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
    // Abort API
    // ============================================================================

    class AbortController {
        readonly signal: AbortSignal;
        abort(reason?: any): void;
    }

    interface AbortSignal extends EventTarget {
        readonly aborted: boolean;
        readonly reason: any;
        onabort: ((this: AbortSignal, ev: Event) => any) | null;
        throwIfAborted(): void;
    }

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
}

export {};
