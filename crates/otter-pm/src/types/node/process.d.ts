/**
 * The `process` object is a global that provides information about, and control
 * over, the current Otter process.
 *
 * In Otter, process.env is isolated and secure by default - it only exposes
 * explicitly configured environment variables, blocking access to secrets.
 *
 * @module process
 */

/**
 * Memory usage information returned by process.memoryUsage().
 */
interface MemoryUsage {
    /** Resident Set Size - total memory allocated for the process */
    rss: number;
    /** Total size of the allocated heap */
    heapTotal: number;
    /** Actual memory used during execution */
    heapUsed: number;
    /** Memory used by C++ objects bound to JavaScript */
    external: number;
    /** Memory allocated for ArrayBuffers and SharedArrayBuffers */
    arrayBuffers: number;
}

/**
 * Version information object.
 */
interface ProcessVersions {
    /** Otter version */
    otter: string;
    /** Node.js compatible version string */
    node: string;
    /** JavaScriptCore version */
    jsc: string;
}

/**
 * High-resolution time methods.
 */
interface HRTime {
    /**
     * Returns the current high-resolution timestamp as a BigInt.
     * The timestamp is relative to an arbitrary time in the past.
     * @example
     * ```ts
     * const start = process.hrtime.bigint();
     * // ... operation
     * const end = process.hrtime.bigint();
     * console.log(`Elapsed: ${end - start} nanoseconds`);
     * ```
     */
    bigint(): bigint;
}

/**
 * Standard output stream interface.
 */
interface WriteStream {
    /**
     * Write data to the stream.
     * @param data The data to write
     */
    write(data: string): void;
}

/**
 * The process object provides information about, and control over, the current process.
 *
 * @example
 * ```ts
 * // Environment variables (isolated and secure)
 * console.log(process.env.NODE_ENV);  // Only shows allowed vars
 *
 * // Process information
 * console.log(process.platform);  // 'darwin', 'linux', 'win32'
 * console.log(process.arch);      // 'x64', 'arm64'
 *
 * // Command line arguments
 * console.log(process.argv);      // ['otter', 'script.ts', '--flag']
 * ```
 */
interface Process {
    /**
     * The process.env property returns an object containing the user environment.
     *
     * **Security Note**: In Otter, this is an isolated environment store that:
     * - Only exposes explicitly configured variables
     * - Blocks access to sensitive env vars by default (AWS keys, tokens, etc.)
     * - Cannot be modified by JavaScript code
     *
     * @example
     * ```ts
     * console.log(process.env.NODE_ENV);  // 'production' (if configured)
     * console.log(process.env.AWS_SECRET); // undefined (blocked)
     * Object.keys(process.env);            // Only shows allowed vars
     * ```
     */
    readonly env: Readonly<Record<string, string | undefined>>;

    /**
     * The process.argv property returns an array containing the command-line
     * arguments passed when the Otter process was launched.
     *
     * @example
     * ```ts
     * // otter run app.ts --port 3000
     * console.log(process.argv);
     * // ['otter', 'app.ts', '--port', '3000']
     * ```
     */
    readonly argv: string[];

    /**
     * Returns the current working directory of the Otter process.
     */
    cwd(): string;

    /**
     * Changes the current working directory.
     * **Note**: Not supported in Otter - will throw an error.
     */
    chdir(directory: string): never;

    /**
     * Terminates the process with the specified exit code.
     * **Note**: In Otter, this throws an error instead of terminating.
     * @param code The exit code (default: 0)
     */
    exit(code?: number): never;

    /**
     * The process.pid property returns the PID of the process.
     */
    readonly pid: number;

    /**
     * The process.ppid property returns the PID of the parent process.
     */
    readonly ppid: number;

    /**
     * The operating system platform.
     * Possible values: 'darwin', 'linux', 'win32'
     */
    readonly platform: "darwin" | "linux" | "win32" | string;

    /**
     * The operating system CPU architecture.
     * Possible values: 'x64', 'arm64', 'ia32', 'arm'
     */
    readonly arch: "x64" | "arm64" | "ia32" | "arm" | string;

    /**
     * The process.version property contains the Otter version string
     * (formatted as Node.js version for compatibility).
     */
    readonly version: string;

    /**
     * The process.versions property returns an object listing the version
     * strings of Otter and its dependencies.
     */
    readonly versions: ProcessVersions;

    /**
     * Schedules a callback to be invoked in the next iteration of the event loop.
     * @param callback The function to call
     * @param args Arguments to pass to the callback
     */
    nextTick(callback: (...args: any[]) => void, ...args: any[]): void;

    /**
     * Returns the current high-resolution time.
     */
    readonly hrtime: HRTime;

    /**
     * Returns an object describing the memory usage of the Otter process.
     */
    memoryUsage(): MemoryUsage;

    /**
     * The process.stdin property returns a stream connected to stdin.
     * **Note**: Currently null in Otter.
     */
    readonly stdin: null;

    /**
     * The process.stdout property returns a stream connected to stdout.
     */
    readonly stdout: WriteStream;

    /**
     * The process.stderr property returns a stream connected to stderr.
     */
    readonly stderr: WriteStream;

    /**
     * Register an event listener. Currently a stub for compatibility.
     */
    on(event: string, listener: (...args: any[]) => void): void;

    /**
     * Remove an event listener. Currently a stub for compatibility.
     */
    off(event: string, listener: (...args: any[]) => void): void;

    /**
     * Register a one-time event listener. Currently a stub for compatibility.
     */
    once(event: string, listener: (...args: any[]) => void): void;

    /**
     * Emit an event. Currently a stub for compatibility.
     */
    emit(event: string, ...args: any[]): void;

    /**
     * Remove an event listener. Currently a stub for compatibility.
     */
    removeListener(event: string, listener: (...args: any[]) => void): void;

    /**
     * Remove all listeners for an event. Currently a stub for compatibility.
     */
    removeAllListeners(event?: string): void;

    /**
     * Get listeners for an event. Currently returns empty array.
     */
    listeners(event: string): ((...args: any[]) => void)[];

    /**
     * Get listener count for an event. Currently returns 0.
     */
    listenerCount(event: string): number;
}

/**
 * The process object provides information about, and control over, the current
 * Otter process.
 */
declare const process: Process;

declare module "node:process" {
    const process: Process;
    export = process;
}

declare module "process" {
    const process: Process;
    export = process;
}
