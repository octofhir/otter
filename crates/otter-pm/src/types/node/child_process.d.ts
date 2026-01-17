/**
 * The `node:child_process` module provides the ability to spawn subprocesses.
 * Otter provides both Node.js-compatible API and an Otter-native API via `Otter.spawn()`.
 * @module node:child_process
 */
declare module "node:child_process" {
    import { EventEmitter } from "node:events";
    import type { Readable, Writable } from "node:stream";

    /**
     * Stdio configuration for child process streams.
     */
    export type StdioOption = "pipe" | "ignore" | "inherit";

    /**
     * Options for spawn() and related functions.
     */
    export interface SpawnOptions {
        /** Current working directory of the child process */
        cwd?: string;
        /** Environment key-value pairs */
        env?: Record<string, string>;
        /** Child's stdio configuration */
        stdio?: StdioOption | [StdioOption, StdioOption, StdioOption];
        /** Stdin configuration */
        stdin?: StdioOption;
        /** Stdout configuration */
        stdout?: StdioOption;
        /** Stderr configuration */
        stderr?: StdioOption;
        /** Shell to execute the command with */
        shell?: boolean | string;
        /** Timeout in milliseconds */
        timeout?: number;
        /** Detached child process */
        detached?: boolean;
        /** Enable IPC channel for fork() */
        ipc?: boolean;
    }

    /**
     * Options for exec() and execFile().
     */
    export interface ExecOptions extends SpawnOptions {
        /** Encoding for stdout/stderr (default: 'buffer') */
        encoding?: BufferEncoding | "buffer";
        /** Maximum amount of data in bytes allowed on stdout/stderr */
        maxBuffer?: number;
        /** Kill signal on timeout (default: 'SIGTERM') */
        killSignal?: string;
    }

    /**
     * Options for spawnSync() and related functions.
     */
    export interface SpawnSyncOptions extends SpawnOptions {
        /** Input to send to child's stdin */
        input?: string | Buffer;
        /** Encoding for stdout/stderr */
        encoding?: BufferEncoding | "buffer";
        /** Maximum amount of data in bytes allowed on stdout/stderr */
        maxBuffer?: number;
        /** Kill signal on timeout */
        killSignal?: string;
    }

    /**
     * Result from synchronous spawn functions.
     */
    export interface SpawnSyncReturns<T = Buffer> {
        /** The process ID of the spawned process */
        pid?: number;
        /** Captured stdout */
        stdout: T;
        /** Captured stderr */
        stderr: T;
        /** Exit code of the process, or null if killed by signal */
        status: number | null;
        /** Signal that terminated the process, or null */
        signal: string | null;
        /** Error object if the process failed to spawn */
        error?: Error;
    }

    /**
     * Options for fork().
     */
    export interface ForkOptions extends SpawnOptions {
        /** Executable to run (default: process.execPath -> 'otter') */
        execPath?: string;
        /** Additional arguments to pass to the executable */
        execArgv?: string[];
        /** Silent mode - don't pipe child's stdio to parent */
        silent?: boolean;
    }

    /**
     * Callback for exec() and execFile().
     */
    export type ExecCallback = (
        error: Error | null,
        stdout: string | Buffer,
        stderr: string | Buffer
    ) => void;

    /**
     * ChildProcess represents a spawned child process.
     * Extends EventEmitter for event handling.
     *
     * @example
     * ```ts
     * import { spawn } from 'node:child_process';
     *
     * const child = spawn('ls', ['-la']);
     * child.stdout.on('data', (data) => console.log(data.toString()));
     * child.on('exit', (code) => console.log('exited with', code));
     * ```
     */
    export class ChildProcess extends EventEmitter {
        /**
         * The process ID of the child process.
         * Set to undefined if the process failed to spawn.
         */
        readonly pid: number | undefined;

        /**
         * The writable stream connected to the child's stdin.
         * Set to null if stdio was not set to 'pipe'.
         */
        readonly stdin: Writable | null;

        /**
         * The readable stream connected to the child's stdout.
         * Set to null if stdio was not set to 'pipe'.
         */
        readonly stdout: Readable | null;

        /**
         * The readable stream connected to the child's stderr.
         * Set to null if stdio was not set to 'pipe'.
         */
        readonly stderr: Readable | null;

        /**
         * Whether the IPC channel is connected.
         */
        readonly connected: boolean;

        /**
         * The exit code of the child process, or null if not exited.
         */
        readonly exitCode: number | null;

        /**
         * The signal that terminated the process, or null.
         */
        readonly signalCode: string | null;

        /**
         * Whether the child was successfully sent a signal.
         */
        readonly killed: boolean;

        /**
         * Send a signal to the child process.
         * @param signal Signal to send (default: 'SIGTERM')
         * @returns true if signal was successfully sent
         * @example
         * ```ts
         * child.kill('SIGINT');
         * ```
         */
        kill(signal?: string): boolean;

        /**
         * Send a message to the child process over IPC.
         * Only available when spawned with ipc: true or via fork().
         * @param message The message to send (must be JSON-serializable)
         * @param callback Optional callback called when message is sent
         * @example
         * ```ts
         * child.send({ type: 'task', data: [1, 2, 3] });
         * ```
         */
        send(message: any, callback?: (error: Error | null) => void): boolean;

        /**
         * Close the IPC channel to the child.
         */
        disconnect(): void;

        /**
         * Keep the parent process alive while this child is running.
         */
        ref(): this;

        /**
         * Allow the parent process to exit independently of the child.
         */
        unref(): this;

        // Events
        on(event: "spawn", listener: () => void): this;
        on(event: "exit", listener: (code: number | null, signal: string | null) => void): this;
        on(event: "close", listener: (code: number | null, signal: string | null) => void): this;
        on(event: "error", listener: (err: Error) => void): this;
        on(event: "message", listener: (message: any) => void): this;
        on(event: "disconnect", listener: () => void): this;
        on(event: string | symbol, listener: (...args: any[]) => void): this;
    }

    /**
     * Spawn a new process using the given command.
     *
     * @param command The command to run
     * @param args Arguments to pass to the command
     * @param options Spawn options
     * @example
     * ```ts
     * import { spawn } from 'node:child_process';
     *
     * const child = spawn('ls', ['-la', '/tmp']);
     *
     * child.stdout.on('data', (data) => {
     *   console.log(`stdout: ${data}`);
     * });
     *
     * child.on('close', (code) => {
     *   console.log(`child process exited with code ${code}`);
     * });
     * ```
     */
    export function spawn(command: string, args?: string[], options?: SpawnOptions): ChildProcess;
    export function spawn(command: string, options?: SpawnOptions): ChildProcess;

    /**
     * Spawns a shell and executes the command within it.
     *
     * @param command The command to run in the shell
     * @param options Exec options
     * @param callback Called with the output when process terminates
     * @example
     * ```ts
     * import { exec } from 'node:child_process';
     *
     * exec('ls -la', (error, stdout, stderr) => {
     *   if (error) {
     *     console.error(`exec error: ${error}`);
     *     return;
     *   }
     *   console.log(`stdout: ${stdout}`);
     * });
     * ```
     */
    export function exec(command: string, callback?: ExecCallback): ChildProcess;
    export function exec(command: string, options: ExecOptions, callback?: ExecCallback): ChildProcess;

    /**
     * Synchronous version of exec(). Blocks until the process terminates.
     *
     * @param command The command to run
     * @param options Exec options
     * @returns The stdout from the command
     * @throws Error if the command fails
     * @example
     * ```ts
     * import { execSync } from 'node:child_process';
     *
     * const result = execSync('echo hello');
     * console.log(result.toString()); // 'hello\n'
     * ```
     */
    export function execSync(command: string, options?: SpawnSyncOptions): Buffer;
    export function execSync(command: string, options: SpawnSyncOptions & { encoding: BufferEncoding }): string;

    /**
     * Similar to exec() but does not spawn a shell by default.
     *
     * @param file The file to execute
     * @param args Arguments to pass
     * @param options Options
     * @param callback Called with output when process terminates
     */
    export function execFile(file: string, callback?: ExecCallback): ChildProcess;
    export function execFile(file: string, args?: string[], callback?: ExecCallback): ChildProcess;
    export function execFile(file: string, args?: string[], options?: ExecOptions, callback?: ExecCallback): ChildProcess;

    /**
     * Synchronous version of execFile().
     *
     * @param file The file to execute
     * @param args Arguments to pass
     * @param options Options
     * @returns The stdout from the command
     */
    export function execFileSync(file: string, args?: string[], options?: SpawnSyncOptions): Buffer;
    export function execFileSync(file: string, args?: string[], options: SpawnSyncOptions & { encoding: BufferEncoding }): string;

    /**
     * Synchronous version of spawn(). Blocks until the process terminates.
     *
     * @param command The command to run
     * @param args Arguments to pass
     * @param options Options
     * @example
     * ```ts
     * import { spawnSync } from 'node:child_process';
     *
     * const result = spawnSync('ls', ['-la']);
     * console.log(result.stdout.toString());
     * ```
     */
    export function spawnSync(command: string, args?: string[], options?: SpawnSyncOptions): SpawnSyncReturns<Buffer>;
    export function spawnSync(command: string, args?: string[], options: SpawnSyncOptions & { encoding: BufferEncoding }): SpawnSyncReturns<string>;

    /**
     * Spawn a new Otter process running the specified module with IPC enabled.
     *
     * This is similar to spawn() but:
     * - Automatically uses 'otter run' as the command
     * - Sets up an IPC channel between parent and child
     * - Child can use process.send() and process.on('message')
     *
     * @param modulePath Path to the module to run
     * @param args Arguments to pass to the module
     * @param options Fork options
     * @example
     * ```ts
     * import { fork } from 'node:child_process';
     *
     * // parent.ts
     * const child = fork('./worker.ts');
     * child.send({ task: 'compute', data: [1, 2, 3] });
     * child.on('message', (msg) => console.log('Result:', msg));
     *
     * // worker.ts
     * process.on('message', (msg) => {
     *   const result = msg.data.reduce((a, b) => a + b, 0);
     *   process.send({ result });
     * });
     * ```
     */
    export function fork(modulePath: string, args?: string[], options?: ForkOptions): ChildProcess;
    export function fork(modulePath: string, options?: ForkOptions): ChildProcess;
}

// Also support the 'child_process' module (without node: prefix)
declare module "child_process" {
    export * from "node:child_process";
}

/**
 * Otter-native subprocess API.
 * Provides modern async/await-friendly process spawning.
 */
declare namespace Otter {
    /**
     * Options for Otter.spawn().
     */
    interface SpawnOptions {
        /** Current working directory */
        cwd?: string;
        /** Environment variables */
        env?: Record<string, string>;
        /** Stdin configuration */
        stdin?: "pipe" | "ignore" | "inherit";
        /** Stdout configuration */
        stdout?: "pipe" | "ignore" | "inherit";
        /** Stderr configuration */
        stderr?: "pipe" | "ignore" | "inherit";
        /** Enable IPC channel */
        ipc?: boolean;
        /** Shell to use for command execution */
        shell?: boolean | string;
        /** Timeout in milliseconds */
        timeout?: number;
        /** Callback when process exits */
        onExit?: (proc: Subprocess, exitCode: number | null, signalCode: string | null, error: Error | null) => void;
    }

    /**
     * Result from Otter.spawnSync().
     */
    interface SpawnSyncResult {
        /** Process ID */
        pid: number | undefined;
        /** Captured stdout as Buffer */
        stdout: Buffer;
        /** Captured stderr as Buffer */
        stderr: Buffer;
        /** Exit code, or null if killed by signal */
        status: number | null;
        /** Signal that killed the process, or null */
        signal: string | null;
        /** Error if process failed to spawn */
        error: Error | null;
    }

    /**
     * A spawned subprocess (Otter-native API).
     *
     * @example
     * ```ts
     * const proc = Otter.spawn(['echo', 'hello']);
     * const output = await proc.stdout.text();
     * console.log(output); // 'hello\n'
     * await proc.exited;
     * ```
     */
    interface Subprocess {
        /** Process ID */
        readonly pid: number | undefined;

        /** Promise that resolves to exit code when process exits */
        readonly exited: Promise<number>;

        /** Exit code after process exits, or null */
        readonly exitCode: number | null;

        /** Signal that killed the process, or null */
        readonly signalCode: string | null;

        /** Stdin as WritableStream (if stdio is 'pipe') */
        readonly stdin: WritableStream<Uint8Array> | null;

        /** Stdout as ReadableStream with .text() helper (if stdio is 'pipe') */
        readonly stdout: (ReadableStream<Uint8Array> & { text(): Promise<string> }) | null;

        /** Stderr as ReadableStream with .text() helper (if stdio is 'pipe') */
        readonly stderr: (ReadableStream<Uint8Array> & { text(): Promise<string> }) | null;

        /**
         * Send a signal to the process.
         * @param signal Signal to send (default: 'SIGTERM')
         */
        kill(signal?: string): boolean;

        /** Keep the event loop alive while this process runs */
        ref(): this;

        /** Allow the event loop to exit while this process runs */
        unref(): this;
    }

    /**
     * Spawn a subprocess (Otter-native API).
     *
     * This is the recommended way to spawn processes in Otter.
     * Uses modern Web Streams API for stdout/stderr.
     *
     * @param cmd Command and arguments as an array
     * @param options Spawn options
     * @example
     * ```ts
     * // Simple command
     * const proc = Otter.spawn(['echo', 'hello']);
     * const output = await proc.stdout.text();
     *
     * // With options
     * const proc = Otter.spawn(['node', 'script.js'], {
     *   cwd: '/tmp',
     *   env: { NODE_ENV: 'production' },
     *   onExit(proc, code) {
     *     console.log('Process exited with', code);
     *   }
     * });
     *
     * // Stream processing
     * const proc = Otter.spawn(['cat', 'large-file.txt']);
     * for await (const chunk of proc.stdout) {
     *   process(chunk);
     * }
     * ```
     */
    function spawn(cmd: string[], options?: SpawnOptions): Subprocess;

    /**
     * Synchronously spawn a subprocess.
     *
     * Blocks until the process terminates.
     *
     * @param cmd Command and arguments as an array
     * @param options Spawn options
     * @example
     * ```ts
     * const result = Otter.spawnSync(['echo', 'hello']);
     * console.log(result.stdout.toString()); // 'hello\n'
     * ```
     */
    function spawnSync(cmd: string[], options?: SpawnOptions): SpawnSyncResult;
}
