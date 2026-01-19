// child_process module - Node.js compatible process spawning

(function() {
    'use strict';

    const EventEmitter = globalThis.__EventEmitter || class EventEmitter {
        constructor() { this._events = new Map(); }
        on(event, listener) {
            if (!this._events.has(event)) this._events.set(event, []);
            this._events.get(event).push(listener);
            return this;
        }
        emit(event, ...args) {
            const listeners = this._events.get(event) || [];
            listeners.forEach(fn => fn(...args));
            return listeners.length > 0;
        }
        removeListener(event, listener) {
            const listeners = this._events.get(event) || [];
            const idx = listeners.indexOf(listener);
            if (idx !== -1) listeners.splice(idx, 1);
            return this;
        }
    };

    // Track all processes for polling
    const processes = new Map();

    // === Subprocess class (Otter-native) ===
    class Subprocess {
        constructor(id, options = {}) {
            this._id = id;
            this._pid = cpPid(id);
            this._exitCode = null;
            this._signalCode = null;
            this._exited = false;
            this._onExit = options.onExit;

            // Create exited promise
            this._exitedPromise = new Promise((resolve, reject) => {
                this._resolveExited = resolve;
                this._rejectExited = reject;
            });

            // stdout as ReadableStream (if piped)
            if (options.stdout !== 'ignore' && options.stdout !== 'inherit') {
                this._stdoutChunks = [];
                this.stdout = new ReadableStream({
                    start: (controller) => {
                        this._stdoutController = controller;
                    }
                });
                // Add text() helper
                this.stdout.text = async () => {
                    const reader = this.stdout.getReader();
                    const chunks = [];
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                    const decoder = new TextDecoder();
                    return chunks.map(c => decoder.decode(c)).join('');
                };
            } else {
                this.stdout = null;
            }

            // stderr as ReadableStream (if piped)
            if (options.stderr !== 'ignore' && options.stderr !== 'inherit') {
                this.stderr = new ReadableStream({
                    start: (controller) => {
                        this._stderrController = controller;
                    }
                });
                this.stderr.text = async () => {
                    const reader = this.stderr.getReader();
                    const chunks = [];
                    while (true) {
                        const { done, value } = await reader.read();
                        if (done) break;
                        chunks.push(value);
                    }
                    const decoder = new TextDecoder();
                    return chunks.map(c => decoder.decode(c)).join('');
                };
            } else {
                this.stderr = null;
            }

            // stdin as WritableStream (if piped)
            if (options.stdin !== 'ignore' && options.stdin !== 'inherit') {
                const id = this._id;
                this.stdin = new WritableStream({
                    write(chunk) {
                        let data;
                        if (typeof chunk === 'string') {
                            data = new TextEncoder().encode(chunk);
                        } else {
                            data = chunk;
                        }
                        cpWriteStdin(id, Array.from(data));
                    },
                    close() {
                        cpCloseStdin(id);
                    }
                });
            } else {
                this.stdin = null;
            }

            processes.set(id, this);
        }

        get pid() { return this._pid; }
        get exited() { return this._exitedPromise; }
        get exitCode() { return this._exitCode; }
        get signalCode() { return this._signalCode; }

        kill(signal) {
            return cpKill(this._id, signal);
        }

        ref() {
            cpRef(this._id);
            return this;
        }

        unref() {
            cpUnref(this._id);
            return this;
        }

        _handleEvent(event) {
            switch (event.type) {
                case 'stdout':
                    if (this._stdoutController) {
                        const data = event.data.data || event.data;
                        this._stdoutController.enqueue(new Uint8Array(data));
                    }
                    break;
                case 'stderr':
                    if (this._stderrController) {
                        const data = event.data.data || event.data;
                        this._stderrController.enqueue(new Uint8Array(data));
                    }
                    break;
                case 'exit':
                    this._exitCode = event.code;
                    this._signalCode = event.signal;
                    break;
                case 'close':
                    this._exited = true;
                    if (this._stdoutController) {
                        try { this._stdoutController.close(); } catch {}
                    }
                    if (this._stderrController) {
                        try { this._stderrController.close(); } catch {}
                    }
                    if (this._onExit) {
                        this._onExit(this, this._exitCode, this._signalCode, null);
                    }
                    this._resolveExited(this._exitCode ?? 0);
                    processes.delete(this._id);
                    break;
                case 'error':
                    if (this._onExit) {
                        this._onExit(this, null, null, new Error(event.message));
                    }
                    this._rejectExited(new Error(event.message));
                    processes.delete(this._id);
                    break;
            }
        }
    }

    // === ChildProcess class (Node.js-style) ===
    class ChildProcess extends EventEmitter {
        constructor(id) {
            super();
            this._id = id;
            this._pid = cpPid(id);
            this._exitCode = null;
            this._signalCode = null;
            this._killed = false;

            // Create stream-like objects
            this.stdin = new ChildStdin(id);
            this.stdout = new ChildReadable(id, 'stdout');
            this.stderr = new ChildReadable(id, 'stderr');

            processes.set(id, this);
        }

        get pid() { return this._pid; }
        get exitCode() { return this._exitCode; }
        get signalCode() { return this._signalCode; }
        get killed() { return this._killed; }

        kill(signal) {
            const result = cpKill(this._id, signal);
            if (result) this._killed = true;
            return result;
        }

        ref() {
            cpRef(this._id);
            return this;
        }

        unref() {
            cpUnref(this._id);
            return this;
        }

        _handleEvent(event) {
            switch (event.type) {
                case 'spawn':
                    this.emit('spawn');
                    break;
                case 'stdout':
                    const stdoutData = event.data.data || event.data;
                    this.stdout._push(Buffer.from(stdoutData));
                    break;
                case 'stderr':
                    const stderrData = event.data.data || event.data;
                    this.stderr._push(Buffer.from(stderrData));
                    break;
                case 'exit':
                    this._exitCode = event.code;
                    this._signalCode = event.signal;
                    this.emit('exit', event.code, event.signal);
                    break;
                case 'close':
                    this.emit('close', event.code, event.signal);
                    processes.delete(this._id);
                    break;
                case 'error':
                    this.emit('error', new Error(event.message));
                    processes.delete(this._id);
                    break;
                case 'message':
                    this.emit('message', event.data);
                    break;
            }
        }

        send(message) {
            // IPC send - to be implemented
            return true;
        }
    }

    // Stdin stream for ChildProcess
    class ChildStdin extends EventEmitter {
        constructor(id) {
            super();
            this._id = id;
            this._ended = false;
        }

        write(data, encoding, callback) {
            if (this._ended) throw new Error('write after end');
            let buffer;
            if (typeof data === 'string') {
                buffer = Buffer.from(data, encoding || 'utf8');
            } else {
                buffer = data;
            }
            cpWriteStdin(this._id, Array.from(buffer));
            if (callback) queueMicrotask(callback);
            return true;
        }

        end(data, encoding, callback) {
            if (data) this.write(data, encoding);
            this._ended = true;
            cpCloseStdin(this._id);
            if (callback) queueMicrotask(callback);
        }
    }

    // Readable stream for stdout/stderr
    class ChildReadable extends EventEmitter {
        constructor(id, type) {
            super();
            this._id = id;
            this._type = type;
        }

        _push(data) {
            this.emit('data', data);
        }
    }

    // === Public API ===

    // Otter.spawn (Otter-native)
    function otterSpawn(cmd, options = {}) {
        const id = cpSpawn(cmd, options);
        return new Subprocess(id, options);
    }

    // Otter.spawnSync (Otter-native)
    function otterSpawnSync(cmd, options = {}) {
        const result = cpSpawnSync(cmd, options);
        return {
            pid: result.pid,
            stdout: result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]),
            stderr: result.stderr.data ? Buffer.from(result.stderr.data) : Buffer.from([]),
            status: result.status,
            signal: result.signal,
            error: result.error ? new Error(result.error) : null,
        };
    }

    // Node.js spawn
    function spawn(command, args, options) {
        if (Array.isArray(args)) {
            // spawn(command, args, options)
        } else if (args && typeof args === 'object') {
            options = args;
            args = [];
        } else {
            args = [];
            options = {};
        }

        const cmd = [command, ...(args || [])];
        const id = cpSpawn(cmd, options || {});
        return new ChildProcess(id);
    }

    // Node.js exec
    function exec(command, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = {};
        }
        options = options || {};

        const child = spawn(command, [], {
            ...options,
            shell: options.shell !== false ? (typeof options.shell === 'string' ? options.shell : '/bin/sh') : null,
        });

        let stdout = [];
        let stderr = [];

        child.stdout.on('data', (data) => stdout.push(data));
        child.stderr.on('data', (data) => stderr.push(data));

        child.on('close', (code, signal) => {
            const stdoutBuf = Buffer.concat(stdout);
            const stderrBuf = Buffer.concat(stderr);
            if (callback) {
                const err = code !== 0
                    ? Object.assign(new Error(`Command failed: ${command}`), { code, signal })
                    : null;
                callback(err, stdoutBuf, stderrBuf);
            }
        });

        child.on('error', (err) => {
            if (callback) callback(err, '', '');
        });

        return child;
    }

    // Node.js execSync
    function execSync(command, options) {
        options = options || {};
        const shell = options.shell !== false
            ? (typeof options.shell === 'string' ? options.shell : '/bin/sh')
            : null;

        const result = cpSpawnSync([command], { ...options, shell });

        if (result.error) {
            throw new Error(result.error);
        }

        if (result.status !== 0) {
            const err = new Error(`Command failed: ${command}`);
            err.status = result.status;
            err.signal = result.signal;
            err.stderr = result.stderr.data ? Buffer.from(result.stderr.data) : Buffer.from([]);
            throw err;
        }

        const stdout = result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]);
        if (options.encoding === 'utf8' || options.encoding === 'utf-8') {
            return stdout.toString('utf8');
        }
        return stdout;
    }

    // Node.js spawnSync
    function spawnSync(command, args, options) {
        if (Array.isArray(args)) {
            // spawnSync(command, args, options)
        } else if (args && typeof args === 'object') {
            options = args;
            args = [];
        } else {
            args = [];
            options = {};
        }

        const cmd = [command, ...(args || [])];
        return otterSpawnSync(cmd, options || {});
    }

    // Node.js execFile
    function execFile(file, args, options, callback) {
        if (typeof args === 'function') {
            callback = args;
            args = [];
            options = {};
        } else if (typeof options === 'function') {
            callback = options;
            options = {};
        }

        const child = spawn(file, args, options);

        let stdout = [];
        let stderr = [];

        child.stdout.on('data', (data) => stdout.push(data));
        child.stderr.on('data', (data) => stderr.push(data));

        child.on('close', (code, signal) => {
            const stdoutBuf = Buffer.concat(stdout);
            const stderrBuf = Buffer.concat(stderr);
            if (callback) {
                const err = code !== 0
                    ? Object.assign(new Error(`Command failed: ${file}`), { code, signal })
                    : null;
                callback(err, stdoutBuf, stderrBuf);
            }
        });

        return child;
    }

    // Node.js execFileSync
    function execFileSync(file, args, options) {
        if (!Array.isArray(args)) {
            options = args;
            args = [];
        }

        const cmd = [file, ...(args || [])];
        const result = cpSpawnSync(cmd, options || {});

        if (result.error) {
            throw new Error(result.error);
        }

        if (result.status !== 0) {
            const err = new Error(`Command failed: ${file}`);
            err.status = result.status;
            err.signal = result.signal;
            throw err;
        }

        return result.stdout.data ? Buffer.from(result.stdout.data) : Buffer.from([]);
    }

    // Node.js fork
    function fork(modulePath, args, options) {
        if (!Array.isArray(args)) {
            options = args;
            args = [];
        }
        options = options || {};

        // fork is spawn with ipc: true and running otter
        const execPath = options.execPath || 'otter';
        const execArgs = options.execArgv || [];

        const cmd = [execPath, ...execArgs, 'run', modulePath, ...(args || [])];
        const id = cpSpawn(cmd, { ...options, ipc: true });
        return new ChildProcess(id);
    }

    // Event loop polling
    globalThis.__otter_cp_poll = function() {
        const events = cpPollEvents();
        for (const event of events) {
            const proc = processes.get(event.id);
            if (proc) {
                proc._handleEvent(event);
            }
        }
        return events.length;
    };

    // Register Otter.spawn
    if (!globalThis.Otter) globalThis.Otter = {};
    globalThis.Otter.spawn = otterSpawn;
    globalThis.Otter.spawnSync = otterSpawnSync;

    // Export child_process module
    const childProcessModule = {
        spawn,
        spawnSync,
        exec,
        execSync,
        execFile,
        execFileSync,
        fork,
        ChildProcess,
    };

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('child_process', childProcessModule);
    }
})();
