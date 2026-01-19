(function() {
    'use strict';

    // Debug: log extension loading (uncomment for debugging)
    // console.log('[node_stream] Loading Node.js stream module...');

    // Use existing EventEmitter - DO NOT create a new implementation
    const EventEmitter = globalThis.__EventEmitter;
    if (!EventEmitter) {
        console.error('[node_stream] ERROR: EventEmitter not found!');
        return; // Don't throw, just skip registration
    }

    // Default high water marks
    let defaultHighWaterMark = 16384; // 16KB for buffer mode
    let defaultObjectHighWaterMark = 16; // 16 objects for object mode

    function getDefaultHighWaterMark(objectMode) {
        return objectMode ? defaultObjectHighWaterMark : defaultHighWaterMark;
    }

    function setDefaultHighWaterMark(objectMode, value) {
        if (typeof value !== 'number' || value < 0) {
            throw new TypeError('highWaterMark must be a non-negative number');
        }
        if (objectMode) {
            defaultObjectHighWaterMark = value;
        } else {
            defaultHighWaterMark = value;
        }
    }

    // Symbols for internal state
    const kDestroyed = Symbol('destroyed');
    const kErrored = Symbol('errored');

    // ============================================================================
    // Stream - Base class
    // ============================================================================
    class Stream extends EventEmitter {
        constructor(options = {}) {
            super();
            this[kDestroyed] = false;
            this[kErrored] = null;
        }

        get destroyed() {
            return this[kDestroyed];
        }

        get errored() {
            return this[kErrored];
        }

        destroy(err) {
            if (this[kDestroyed]) return this;
            this[kDestroyed] = true;

            if (err) {
                this[kErrored] = err;
                process.nextTick(() => this.emit('error', err));
            }

            process.nextTick(() => this.emit('close'));
            return this;
        }
    }

    // ============================================================================
    // Readable - Read stream
    // ============================================================================
    class Readable extends Stream {
        constructor(options = {}) {
            super(options);

            const hwm = options.highWaterMark ?? options.readableHighWaterMark;
            this.readableHighWaterMark = hwm ?? getDefaultHighWaterMark(options.objectMode);
            this.readableObjectMode = options.objectMode ?? false;
            this.readableEncoding = options.encoding ?? null;

            this._readableState = {
                buffer: [],
                length: 0,
                flowing: null, // null | true | false (three states)
                ended: false,
                endEmitted: false,
                reading: false,
                paused: true,
                pipes: [],
                needReadable: false,
                emittedReadable: false,
            };

            if (typeof options.read === 'function') {
                this._read = options.read.bind(this);
            }
            if (typeof options.destroy === 'function') {
                this._destroy = options.destroy.bind(this);
            }
        }

        // Override on() to auto-start flowing mode when 'data' listener is added
        on(event, listener) {
            const result = super.on(event, listener);

            if (event === 'data') {
                // Auto-start flowing mode when data listener is added
                if (this._readableState.flowing !== false) {
                    this.resume();
                }
            } else if (event === 'readable') {
                // If readable listener is added, start reading
                const state = this._readableState;
                if (!state.endEmitted && !state.reading) {
                    state.reading = true;
                    this._read(this.readableHighWaterMark);
                    state.reading = false;
                }
            }

            return result;
        }

        // Alias for on
        addListener(event, listener) {
            return this.on(event, listener);
        }

        get readable() {
            const state = this._readableState;
            return !this[kDestroyed] && !state.endEmitted;
        }

        get readableLength() {
            return this._readableState.length;
        }

        get readableFlowing() {
            return this._readableState.flowing;
        }

        get readableEnded() {
            return this._readableState.ended;
        }

        get readableAborted() {
            return this[kDestroyed] && !this._readableState.endEmitted;
        }

        // Override in subclass to provide data
        _read(size) {
            // Default: do nothing, subclass should override
        }

        // Push data into the read buffer
        push(chunk, encoding) {
            const state = this._readableState;

            if (state.ended) {
                this.emit('error', new Error('stream.push() after EOF'));
                return false;
            }

            // null signals EOF
            if (chunk === null) {
                state.ended = true;
                if (state.length === 0) {
                    this._endReadable();
                }
                return false;
            }

            // Convert string to Buffer if not in object mode
            if (!this.readableObjectMode && typeof chunk === 'string') {
                encoding = encoding || this.readableEncoding || 'utf8';
                chunk = Buffer.from(chunk, encoding);
            }

            const len = this.readableObjectMode ? 1 : (chunk.length || chunk.byteLength || 0);
            state.buffer.push(chunk);
            state.length += len;

            // Emit events based on flowing state
            if (state.flowing) {
                this._flow();
            } else if (state.needReadable) {
                state.needReadable = false;
                if (!state.emittedReadable) {
                    state.emittedReadable = true;
                    process.nextTick(() => {
                        state.emittedReadable = false;
                        this.emit('readable');
                    });
                }
            }

            return state.length < this.readableHighWaterMark;
        }

        // Unshift data back to the front of the buffer
        unshift(chunk, encoding) {
            const state = this._readableState;

            if (chunk === null) {
                return false;
            }

            if (!this.readableObjectMode && typeof chunk === 'string') {
                encoding = encoding || this.readableEncoding || 'utf8';
                chunk = Buffer.from(chunk, encoding);
            }

            const len = this.readableObjectMode ? 1 : (chunk.length || chunk.byteLength || 0);
            state.buffer.unshift(chunk);
            state.length += len;

            return true;
        }

        // Read data from the buffer
        read(size) {
            const state = this._readableState;

            if (size === undefined || size === null || size === 0) {
                size = state.length;
            }

            if (size === 0) {
                if (state.ended && state.length === 0) {
                    this._endReadable();
                }
                return null;
            }

            // If no data available, trigger _read
            if (state.length === 0) {
                state.needReadable = true;
                if (!state.reading && !state.ended) {
                    state.reading = true;
                    this._read(this.readableHighWaterMark);
                    state.reading = false;
                }
                return null;
            }

            let chunk;
            if (this.readableObjectMode) {
                chunk = state.buffer.shift();
                state.length--;
            } else {
                if (size >= state.length) {
                    // Return all data
                    if (state.buffer.length === 1) {
                        chunk = state.buffer[0];
                        state.buffer = [];
                    } else {
                        chunk = Buffer.concat(state.buffer);
                        state.buffer = [];
                    }
                    state.length = 0;
                } else {
                    // Return partial data
                    const chunks = [];
                    let remaining = size;
                    while (remaining > 0 && state.buffer.length > 0) {
                        const buf = state.buffer[0];
                        if (buf.length <= remaining) {
                            chunks.push(state.buffer.shift());
                            remaining -= buf.length;
                        } else {
                            chunks.push(buf.slice(0, remaining));
                            state.buffer[0] = buf.slice(remaining);
                            remaining = 0;
                        }
                    }
                    chunk = Buffer.concat(chunks);
                    state.length -= size;
                }
            }

            if (state.ended && state.length === 0) {
                this._endReadable();
            }

            // Encode if encoding is set
            if (chunk && this.readableEncoding && !this.readableObjectMode) {
                chunk = chunk.toString(this.readableEncoding);
            }

            return chunk;
        }

        _flow() {
            const state = this._readableState;
            while (state.flowing && state.buffer.length > 0) {
                const chunk = this.read();
                if (chunk === null) break;
                this.emit('data', chunk);
            }

            // If ended and no more data, end the stream
            if (state.ended && state.length === 0) {
                this._endReadable();
            }
        }

        _endReadable() {
            const state = this._readableState;
            if (!state.endEmitted) {
                state.endEmitted = true;
                process.nextTick(() => this.emit('end'));
            }
        }

        // Pipe to a writable stream
        pipe(dest, options = {}) {
            const state = this._readableState;
            state.pipes.push(dest);

            const onData = (chunk) => {
                const ret = dest.write(chunk);
                if (ret === false) {
                    this.pause();
                }
            };

            const onDrain = () => {
                this.resume();
            };

            const onEnd = () => {
                if (options.end !== false) {
                    dest.end();
                }
            };

            const cleanup = () => {
                this.off('data', onData);
                dest.off('drain', onDrain);
                this.off('end', onEnd);
            };

            this.on('data', onData);
            dest.on('drain', onDrain);
            this.on('end', onEnd);

            dest.on('close', cleanup);
            dest.on('error', cleanup);

            dest.emit('pipe', this);
            this.resume();

            return dest;
        }

        // Remove a pipe
        unpipe(dest) {
            const state = this._readableState;
            if (dest) {
                const idx = state.pipes.indexOf(dest);
                if (idx !== -1) state.pipes.splice(idx, 1);
                dest.emit('unpipe', this);
            } else {
                for (const d of state.pipes) {
                    d.emit('unpipe', this);
                }
                state.pipes = [];
            }
            return this;
        }

        // Pause the stream (switch to paused mode)
        pause() {
            const state = this._readableState;
            if (state.flowing !== false) {
                state.flowing = false;
                state.paused = true;
                this.emit('pause');
            }
            return this;
        }

        // Resume the stream (switch to flowing mode)
        resume() {
            const state = this._readableState;
            if (!state.flowing) {
                state.flowing = true;
                state.paused = false;
                this.emit('resume');
                this._flow();
                if (!state.reading && !state.ended) {
                    state.reading = true;
                    this._read(this.readableHighWaterMark);
                    state.reading = false;
                }
            }
            return this;
        }

        isPaused() {
            return this._readableState.flowing === false;
        }

        setEncoding(encoding) {
            this.readableEncoding = encoding;
            return this;
        }

        // Wrap a legacy stream
        wrap(stream) {
            const state = this._readableState;

            stream.on('data', (chunk) => {
                if (!this.push(chunk)) {
                    stream.pause();
                }
            });

            stream.on('end', () => {
                this.push(null);
            });

            stream.on('error', (err) => {
                this.destroy(err);
            });

            stream.on('close', () => {
                this.destroy();
            });

            this._read = () => {
                if (stream.resume) stream.resume();
            };

            return this;
        }

        // Async iteration support
        [Symbol.asyncIterator]() {
            const stream = this;
            const state = this._readableState;
            let pendingResolve = null;
            let pendingReject = null;
            let ended = false;
            let error = null;

            const onData = (chunk) => {
                if (pendingResolve) {
                    const resolve = pendingResolve;
                    pendingResolve = null;
                    pendingReject = null;
                    resolve({ value: chunk, done: false });
                    stream.pause();
                }
            };

            const onEnd = () => {
                ended = true;
                if (pendingResolve) {
                    const resolve = pendingResolve;
                    pendingResolve = null;
                    pendingReject = null;
                    resolve({ done: true });
                }
            };

            const onError = (err) => {
                error = err;
                if (pendingReject) {
                    const reject = pendingReject;
                    pendingResolve = null;
                    pendingReject = null;
                    reject(err);
                }
            };

            stream.on('data', onData);
            stream.once('end', onEnd);
            stream.once('error', onError);

            return {
                next() {
                    if (error) return Promise.reject(error);
                    if (ended) return Promise.resolve({ done: true });

                    stream.resume();
                    return new Promise((resolve, reject) => {
                        pendingResolve = resolve;
                        pendingReject = reject;
                    });
                },
                return() {
                    stream.off('data', onData);
                    stream.off('end', onEnd);
                    stream.off('error', onError);
                    stream.destroy();
                    return Promise.resolve({ done: true });
                },
                throw(err) {
                    stream.destroy(err);
                    return Promise.reject(err);
                },
                [Symbol.asyncIterator]() {
                    return this;
                }
            };
        }

        // Static: Create from iterable
        static from(iterable, options = {}) {
            if (iterable == null) {
                throw new TypeError('The "iterable" argument must be an iterable');
            }

            const readable = new Readable({
                objectMode: options.objectMode !== false,
                highWaterMark: options.highWaterMark ?? 16,
                ...options
            });

            let iterator;
            let isAsync = false;

            if (typeof iterable[Symbol.asyncIterator] === 'function') {
                iterator = iterable[Symbol.asyncIterator]();
                isAsync = true;
            } else if (typeof iterable[Symbol.iterator] === 'function') {
                iterator = iterable[Symbol.iterator]();
            } else if (iterable instanceof Promise || (iterable && typeof iterable.then === 'function')) {
                iterable.then(
                    (val) => {
                        const r = Readable.from(val, options);
                        r.on('data', (chunk) => readable.push(chunk));
                        r.on('end', () => readable.push(null));
                        r.on('error', (err) => readable.destroy(err));
                    },
                    (err) => readable.destroy(err)
                );
                return readable;
            } else {
                throw new TypeError('The "iterable" argument must be an iterable');
            }

            readable._read = function() {
                const pull = () => {
                    const result = iterator.next();
                    if (isAsync || result instanceof Promise) {
                        Promise.resolve(result).then(
                            ({ value, done }) => {
                                if (done) {
                                    readable.push(null);
                                } else {
                                    if (readable.push(value)) {
                                        pull();
                                    }
                                }
                            },
                            (err) => readable.destroy(err)
                        );
                    } else {
                        if (result.done) {
                            readable.push(null);
                        } else {
                            if (readable.push(result.value)) {
                                pull();
                            }
                        }
                    }
                };
                pull();
            };

            return readable;
        }

        // Static: Convert Web ReadableStream to Node Readable
        static fromWeb(webStream, options = {}) {
            const readable = new Readable(options);
            const reader = webStream.getReader();

            readable._read = async function() {
                try {
                    const { value, done } = await reader.read();
                    if (done) {
                        readable.push(null);
                    } else {
                        readable.push(value);
                    }
                } catch (err) {
                    readable.destroy(err);
                }
            };

            return readable;
        }

        // Static: Convert Node Readable to Web ReadableStream
        static toWeb(nodeReadable) {
            return new ReadableStream({
                start(controller) {
                    nodeReadable.on('data', (chunk) => controller.enqueue(chunk));
                    nodeReadable.on('end', () => controller.close());
                    nodeReadable.on('error', (err) => controller.error(err));
                }
            });
        }

        // Static: Check if stream has been disturbed
        static isDisturbed(stream) {
            return stream._readableState?.reading === true ||
                   stream._readableState?.ended === true ||
                   stream._readableState?.length > 0;
        }
    }

    // ============================================================================
    // Writable - Write stream
    // ============================================================================
    class Writable extends Stream {
        constructor(options = {}) {
            super(options);

            const hwm = options.highWaterMark ?? options.writableHighWaterMark;
            this.writableHighWaterMark = hwm ?? getDefaultHighWaterMark(options.objectMode);
            this.writableObjectMode = options.objectMode ?? false;

            this._writableState = {
                buffer: [],
                length: 0,
                writing: false,
                ended: false,
                finished: false,
                corked: 0,
                needDrain: false,
                finalCalled: false,
                defaultEncoding: 'utf8',
            };

            if (typeof options.write === 'function') {
                this._write = options.write.bind(this);
            }
            if (typeof options.writev === 'function') {
                this._writev = options.writev.bind(this);
            }
            if (typeof options.destroy === 'function') {
                this._destroy = options.destroy.bind(this);
            }
            if (typeof options.final === 'function') {
                this._final = options.final.bind(this);
            }
        }

        get writable() {
            const state = this._writableState;
            return !this[kDestroyed] && !state.ended;
        }

        get writableLength() {
            return this._writableState.length;
        }

        get writableFinished() {
            return this._writableState.finished;
        }

        get writableEnded() {
            return this._writableState.ended;
        }

        get writableCorked() {
            return this._writableState.corked;
        }

        get writableNeedDrain() {
            return this._writableState.needDrain;
        }

        get writableAborted() {
            return this[kDestroyed] && !this._writableState.finished;
        }

        // Override in subclass
        _write(chunk, encoding, callback) {
            callback(new Error('_write() is not implemented'));
        }

        // Batch write (optional override)
        _writev(chunks, callback) {
            let i = 0;
            const writeNext = (err) => {
                if (err) return callback(err);
                if (i >= chunks.length) return callback();
                const { chunk, encoding } = chunks[i++];
                this._write(chunk, encoding, writeNext);
            };
            writeNext();
        }

        // Final callback before finish (optional override)
        _final(callback) {
            callback();
        }

        // Write data
        write(chunk, encoding, callback) {
            if (typeof encoding === 'function') {
                callback = encoding;
                encoding = null;
            }
            callback = callback || (() => {});
            encoding = encoding || this._writableState.defaultEncoding;

            const state = this._writableState;

            if (state.ended) {
                const err = new Error('write after end');
                process.nextTick(() => callback(err));
                this.emit('error', err);
                return false;
            }

            // Convert string to Buffer if not in object mode
            if (!this.writableObjectMode && typeof chunk === 'string') {
                chunk = Buffer.from(chunk, encoding);
            }

            const len = this.writableObjectMode ? 1 : (chunk.length || chunk.byteLength || 0);
            state.length += len;

            const ret = state.length < this.writableHighWaterMark;
            if (!ret) {
                state.needDrain = true;
            }

            if (state.writing || state.corked > 0) {
                state.buffer.push({ chunk, encoding, callback });
            } else {
                this._doWrite(chunk, encoding, callback);
            }

            return ret;
        }

        _doWrite(chunk, encoding, callback) {
            const state = this._writableState;
            state.writing = true;

            this._write(chunk, encoding, (err) => {
                const len = this.writableObjectMode ? 1 : (chunk.length || chunk.byteLength || 0);
                state.length -= len;
                state.writing = false;

                if (err) {
                    callback(err);
                    this.emit('error', err);
                    return;
                }

                callback();

                // Process buffered writes
                if (state.buffer.length > 0 && state.corked === 0) {
                    const item = state.buffer.shift();
                    this._doWrite(item.chunk, item.encoding, item.callback);
                } else if (state.needDrain && state.length === 0) {
                    state.needDrain = false;
                    this.emit('drain');
                }

                // Check if we should finish
                if (state.ended && state.buffer.length === 0 && !state.finished) {
                    this._finish();
                }
            });
        }

        // Signal end of writes
        end(chunk, encoding, callback) {
            if (typeof chunk === 'function') {
                callback = chunk;
                chunk = null;
                encoding = null;
            } else if (typeof encoding === 'function') {
                callback = encoding;
                encoding = null;
            }

            const state = this._writableState;

            if (chunk != null) {
                this.write(chunk, encoding);
            }

            state.ended = true;

            if (callback) {
                this.once('finish', callback);
            }

            if (!state.writing && state.buffer.length === 0) {
                this._finish();
            }

            return this;
        }

        _finish() {
            const state = this._writableState;
            if (state.finished) return;

            const doFinish = () => {
                state.finished = true;
                this.emit('finish');
            };

            if (!state.finalCalled) {
                state.finalCalled = true;
                this._final((err) => {
                    if (err) {
                        this.emit('error', err);
                        return;
                    }
                    doFinish();
                });
            } else {
                doFinish();
            }
        }

        // Buffer writes
        cork() {
            this._writableState.corked++;
        }

        // Flush buffered writes
        uncork() {
            const state = this._writableState;
            if (state.corked > 0) {
                state.corked--;
                if (state.corked === 0 && state.buffer.length > 0 && !state.writing) {
                    const item = state.buffer.shift();
                    this._doWrite(item.chunk, item.encoding, item.callback);
                }
            }
        }

        setDefaultEncoding(encoding) {
            this._writableState.defaultEncoding = encoding;
            return this;
        }

        // Static: Convert Web WritableStream to Node Writable
        static fromWeb(webStream, options = {}) {
            const writable = new Writable(options);
            const writer = webStream.getWriter();

            writable._write = async function(chunk, encoding, callback) {
                try {
                    await writer.write(chunk);
                    callback();
                } catch (err) {
                    callback(err);
                }
            };

            writable._final = async function(callback) {
                try {
                    await writer.close();
                    callback();
                } catch (err) {
                    callback(err);
                }
            };

            return writable;
        }

        // Static: Convert Node Writable to Web WritableStream
        static toWeb(nodeWritable) {
            return new WritableStream({
                write(chunk) {
                    return new Promise((resolve, reject) => {
                        nodeWritable.write(chunk, (err) => {
                            if (err) reject(err);
                            else resolve();
                        });
                    });
                },
                close() {
                    return new Promise((resolve, reject) => {
                        nodeWritable.end((err) => {
                            if (err) reject(err);
                            else resolve();
                        });
                    });
                },
                abort(err) {
                    nodeWritable.destroy(err);
                }
            });
        }
    }

    // ============================================================================
    // Duplex - Both readable and writable
    // ============================================================================
    class Duplex extends Readable {
        constructor(options = {}) {
            super(options);

            // Initialize writable state
            const hwm = options.highWaterMark ?? options.writableHighWaterMark;
            this.writableHighWaterMark = hwm ?? getDefaultHighWaterMark(options.objectMode);
            this.writableObjectMode = options.objectMode ?? options.writableObjectMode ?? false;

            this._writableState = {
                buffer: [],
                length: 0,
                writing: false,
                ended: false,
                finished: false,
                corked: 0,
                needDrain: false,
                finalCalled: false,
                defaultEncoding: 'utf8',
            };

            if (typeof options.write === 'function') {
                this._write = options.write.bind(this);
            }
            if (typeof options.writev === 'function') {
                this._writev = options.writev.bind(this);
            }
            if (typeof options.final === 'function') {
                this._final = options.final.bind(this);
            }

            // Allow disabling one side
            this.allowHalfOpen = options.allowHalfOpen !== false;
            if (options.readable === false) {
                this._readableState.ended = true;
                this._readableState.endEmitted = true;
            }
            if (options.writable === false) {
                this._writableState.ended = true;
                this._writableState.finished = true;
            }
        }

        get writable() {
            const state = this._writableState;
            return !this[kDestroyed] && !state.ended;
        }

        get writableLength() {
            return this._writableState.length;
        }

        get writableFinished() {
            return this._writableState.finished;
        }

        get writableEnded() {
            return this._writableState.ended;
        }

        get writableCorked() {
            return this._writableState.corked;
        }

        get writableNeedDrain() {
            return this._writableState.needDrain;
        }

        get writableAborted() {
            return this[kDestroyed] && !this._writableState.finished;
        }

        // Static: Convert Web streams pair to Duplex
        static fromWeb(pair, options = {}) {
            const duplex = new Duplex(options);

            // Setup readable side
            if (pair.readable) {
                const reader = pair.readable.getReader();
                duplex._read = async function() {
                    try {
                        const { value, done } = await reader.read();
                        if (done) {
                            duplex.push(null);
                        } else {
                            duplex.push(value);
                        }
                    } catch (err) {
                        duplex.destroy(err);
                    }
                };
            }

            // Setup writable side
            if (pair.writable) {
                const writer = pair.writable.getWriter();
                duplex._write = async function(chunk, encoding, callback) {
                    try {
                        await writer.write(chunk);
                        callback();
                    } catch (err) {
                        callback(err);
                    }
                };
                duplex._final = async function(callback) {
                    try {
                        await writer.close();
                        callback();
                    } catch (err) {
                        callback(err);
                    }
                };
            }

            return duplex;
        }

        // Static: Convert Duplex to Web streams pair
        static toWeb(duplex) {
            return {
                readable: Readable.toWeb(duplex),
                writable: Writable.toWeb(duplex),
            };
        }
    }

    // Copy Writable methods to Duplex prototype
    Duplex.prototype._write = Writable.prototype._write;
    Duplex.prototype._writev = Writable.prototype._writev;
    Duplex.prototype._final = Writable.prototype._final;
    Duplex.prototype.write = Writable.prototype.write;
    Duplex.prototype._doWrite = Writable.prototype._doWrite;
    Duplex.prototype.end = Writable.prototype.end;
    Duplex.prototype._finish = Writable.prototype._finish;
    Duplex.prototype.cork = Writable.prototype.cork;
    Duplex.prototype.uncork = Writable.prototype.uncork;
    Duplex.prototype.setDefaultEncoding = Writable.prototype.setDefaultEncoding;

    // ============================================================================
    // Transform - Duplex with transform logic
    // ============================================================================
    class Transform extends Duplex {
        constructor(options = {}) {
            super(options);

            this._transformState = {
                afterTransform: null,
                needTransform: false,
                transforming: false,
                writechunk: null,
                writeencoding: null,
            };

            if (typeof options.transform === 'function') {
                this._transform = options.transform.bind(this);
            }
            if (typeof options.flush === 'function') {
                this._flush = options.flush.bind(this);
            }

            // Override _read to trigger transform
            this._read = (n) => {
                const ts = this._transformState;
                if (ts.writechunk !== null && !ts.transforming) {
                    ts.transforming = true;
                    this._transform(ts.writechunk, ts.writeencoding, ts.afterTransform);
                } else {
                    ts.needTransform = true;
                }
            };
        }

        // Override in subclass
        _transform(chunk, encoding, callback) {
            callback(new Error('_transform() is not implemented'));
        }

        // Called before stream ends
        _flush(callback) {
            callback();
        }

        _write(chunk, encoding, callback) {
            const ts = this._transformState;

            ts.writechunk = chunk;
            ts.writeencoding = encoding;
            ts.afterTransform = (err, data) => {
                ts.transforming = false;
                ts.writechunk = null;
                ts.writeencoding = null;

                if (err) {
                    return callback(err);
                }

                if (data != null) {
                    this.push(data);
                }

                callback();

                if (ts.needTransform) {
                    ts.needTransform = false;
                    this._read();
                }
            };

            if (!ts.transforming) {
                ts.transforming = true;
                this._transform(chunk, encoding, ts.afterTransform);
            }
        }

        _final(callback) {
            this._flush((err, data) => {
                if (err) {
                    return callback(err);
                }
                if (data != null) {
                    this.push(data);
                }
                this.push(null);
                callback();
            });
        }
    }

    // ============================================================================
    // PassThrough - Transform that passes data through unchanged
    // ============================================================================
    class PassThrough extends Transform {
        constructor(options) {
            super(options);
        }

        _transform(chunk, encoding, callback) {
            callback(null, chunk);
        }
    }

    // ============================================================================
    // Utility Functions
    // ============================================================================

    // pipeline - Connect streams with error handling
    function pipeline(...args) {
        const callback = typeof args[args.length - 1] === 'function' ? args.pop() : null;
        const streams = args.flat();

        if (streams.length < 2) {
            throw new Error('pipeline requires at least 2 streams');
        }

        let error;
        const destroys = [];

        function destroyer(stream) {
            let called = false;
            return (err) => {
                if (called) return;
                called = true;
                if (err) error = err;
                stream.destroy(err);
            };
        }

        for (let i = 0; i < streams.length; i++) {
            destroys.push(destroyer(streams[i]));
        }

        function finish(err) {
            for (const destroy of destroys) {
                destroy(err);
            }
            if (callback) {
                callback(err);
            }
        }

        // Pipe all streams
        for (let i = 0; i < streams.length - 1; i++) {
            const src = streams[i];
            const dest = streams[i + 1];

            src.pipe(dest);
            src.on('error', finish);
        }

        // Handle last stream events
        const last = streams[streams.length - 1];
        last.on('finish', () => finish());
        last.on('error', finish);

        return last;
    }

    // finished - Wait for stream to complete
    function finished(stream, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = {};
        }
        options = options || {};

        const readable = options.readable ?? stream.readable !== false;
        const writable = options.writable ?? stream.writable !== false;

        let closed = false;

        const onClose = () => {
            closed = true;
            callback();
        };

        const onError = (err) => {
            callback(err);
        };

        const onEnd = () => {
            if (!writable || closed) {
                callback();
            }
        };

        const onFinish = () => {
            if (!readable || closed) {
                callback();
            }
        };

        stream.on('close', onClose);
        stream.on('error', onError);
        if (readable) stream.on('end', onEnd);
        if (writable) stream.on('finish', onFinish);

        // Return cleanup function
        return () => {
            stream.off('close', onClose);
            stream.off('error', onError);
            stream.off('end', onEnd);
            stream.off('finish', onFinish);
        };
    }

    // compose - Compose multiple streams
    function compose(...streams) {
        streams = streams.flat();
        if (streams.length === 0) {
            throw new Error('compose requires at least one stream');
        }
        if (streams.length === 1) {
            return streams[0];
        }

        const first = streams[0];
        const last = streams[streams.length - 1];

        // Pipe all streams together
        for (let i = 0; i < streams.length - 1; i++) {
            streams[i].pipe(streams[i + 1]);
        }

        // Create a duplex that reads from last and writes to first
        const composed = new Duplex({
            readableObjectMode: last.readableObjectMode,
            writableObjectMode: first.writableObjectMode,
        });

        // Forward writes to first stream
        composed._write = (chunk, encoding, callback) => {
            if (first.write(chunk, encoding)) {
                callback();
            } else {
                first.once('drain', callback);
            }
        };

        composed._final = (callback) => {
            first.end();
            callback();
        };

        // Forward reads from last stream
        last.on('data', (chunk) => {
            if (!composed.push(chunk)) {
                last.pause();
            }
        });

        last.on('end', () => {
            composed.push(null);
        });

        composed._read = () => {
            last.resume();
        };

        // Forward errors
        for (const stream of streams) {
            stream.on('error', (err) => composed.destroy(err));
        }

        return composed;
    }

    // addAbortSignal - Add abort support to stream
    function addAbortSignal(signal, stream) {
        if (!signal || typeof signal.addEventListener !== 'function') {
            throw new TypeError('Invalid signal');
        }

        const onAbort = () => {
            stream.destroy(new Error('The operation was aborted'));
        };

        if (signal.aborted) {
            onAbort();
        } else {
            signal.addEventListener('abort', onAbort, { once: true });
        }

        return stream;
    }

    // isErrored - Check if stream has errored
    function isErrored(stream) {
        return stream[kErrored] != null;
    }

    // isReadable - Check if stream is readable
    function isReadable(stream) {
        return stream instanceof Readable && stream.readable;
    }

    // isWritable - Check if stream is writable
    function isWritable(stream) {
        return stream instanceof Writable && stream.writable;
    }

    // ============================================================================
    // Promises API
    // ============================================================================
    const promises = {
        pipeline: (...args) => new Promise((resolve, reject) => {
            pipeline(...args, (err) => {
                if (err) reject(err);
                else resolve();
            });
        }),
        finished: (stream, options) => new Promise((resolve, reject) => {
            finished(stream, options, (err) => {
                if (err) reject(err);
                else resolve();
            });
        }),
    };

    // ============================================================================
    // Module Export
    // ============================================================================
    const streamModule = {
        Stream,
        Readable,
        Writable,
        Duplex,
        Transform,
        PassThrough,
        pipeline,
        finished,
        compose,
        addAbortSignal,
        getDefaultHighWaterMark,
        setDefaultHighWaterMark,
        isErrored,
        isReadable,
        isWritable,
        promises,
    };

    // Default export for ES module compat
    streamModule.default = streamModule;

    // Register modules
    if (globalThis.__registerModule) {
        globalThis.__registerModule('stream', streamModule);
        globalThis.__registerModule('node:stream', streamModule);
        globalThis.__registerModule('stream/promises', promises);
        globalThis.__registerModule('node:stream/promises', promises);
    }
})();
