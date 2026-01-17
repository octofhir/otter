// Web Streams API wrapper

(function() {
    'use strict';

    // ReadableStreamDefaultReader
    class ReadableStreamDefaultReader {
        constructor(stream) {
            if (stream._locked) {
                throw new TypeError('ReadableStream is locked');
            }
            this._stream = stream;
            this._streamId = stream._id;
            stream._locked = true;
            readableLock(this._streamId);
        }

        async read() {
            return readableRead(this._streamId);
        }

        releaseLock() {
            if (this._stream) {
                this._stream._locked = false;
                readableUnlock(this._streamId);
                this._stream = null;
            }
        }

        cancel(reason) {
            readableError(this._streamId, reason || 'Cancelled');
            return Promise.resolve();
        }

        get closed() {
            return Promise.resolve(); // Simplified
        }
    }

    // ReadableStreamDefaultController
    class ReadableStreamDefaultController {
        constructor(streamId) {
            this._streamId = streamId;
            this._closeRequested = false;
        }

        enqueue(chunk) {
            if (this._closeRequested) {
                throw new TypeError('Cannot enqueue after close');
            }
            readableEnqueue(this._streamId, chunk);
        }

        close() {
            if (this._closeRequested) {
                throw new TypeError('Cannot close twice');
            }
            this._closeRequested = true;
            readableClose(this._streamId);
        }

        error(e) {
            readableError(this._streamId, e ? e.message || String(e) : 'Error');
        }

        get desiredSize() {
            return 1; // Simplified
        }
    }

    // ReadableStream
    class ReadableStream {
        constructor(underlyingSource, strategy) {
            this._id = createReadableStream(strategy?.highWaterMark);
            this._locked = false;
            this._controller = new ReadableStreamDefaultController(this._id);

            // Call start if provided
            if (underlyingSource && underlyingSource.start) {
                underlyingSource.start(this._controller);
            }

            // Store pull and cancel callbacks
            this._pull = underlyingSource?.pull;
            this._cancel = underlyingSource?.cancel;
        }

        get locked() {
            return this._locked;
        }

        getReader(options) {
            if (options && options.mode === 'byob') {
                throw new TypeError('BYOB readers not supported');
            }
            return new ReadableStreamDefaultReader(this);
        }

        cancel(reason) {
            if (this._cancel) {
                this._cancel(reason);
            }
            readableError(this._id, reason || 'Cancelled');
            return Promise.resolve();
        }

        pipeTo(destination, options) {
            const reader = this.getReader();
            const writer = destination.getWriter();

            async function pump() {
                const { value, done } = await reader.read();
                if (done) {
                    writer.close();
                    return;
                }
                writer.write(value);
                return pump();
            }

            return pump();
        }

        pipeThrough(transform, options) {
            this.pipeTo(transform.writable, options);
            return transform.readable;
        }

        tee() {
            // Simplified tee - creates two readable streams
            const stream1 = new ReadableStream();
            const stream2 = new ReadableStream();
            // Not fully implemented
            return [stream1, stream2];
        }
    }

    // WritableStreamDefaultWriter
    class WritableStreamDefaultWriter {
        constructor(stream) {
            if (stream._locked) {
                throw new TypeError('WritableStream is locked');
            }
            this._stream = stream;
            this._streamId = stream._id;
            stream._locked = true;
            writableLock(this._streamId);
        }

        write(chunk) {
            writableWrite(this._streamId, chunk);
            return Promise.resolve();
        }

        close() {
            writableClose(this._streamId);
            return Promise.resolve();
        }

        abort(reason) {
            writableError(this._streamId, reason || 'Aborted');
            return Promise.resolve();
        }

        releaseLock() {
            if (this._stream) {
                this._stream._locked = false;
                writableUnlock(this._streamId);
                this._stream = null;
            }
        }

        get ready() {
            return Promise.resolve();
        }

        get closed() {
            return Promise.resolve();
        }

        get desiredSize() {
            return 1;
        }
    }

    // WritableStream
    class WritableStream {
        constructor(underlyingSink, strategy) {
            this._id = createWritableStream();
            this._locked = false;

            // Store callbacks
            this._write = underlyingSink?.write;
            this._close = underlyingSink?.close;
            this._abort = underlyingSink?.abort;

            // Call start if provided
            if (underlyingSink && underlyingSink.start) {
                underlyingSink.start({ error: (e) => writableError(this._id, e) });
            }
        }

        get locked() {
            return this._locked;
        }

        getWriter() {
            return new WritableStreamDefaultWriter(this);
        }

        abort(reason) {
            if (this._abort) {
                this._abort(reason);
            }
            writableError(this._id, reason || 'Aborted');
            return Promise.resolve();
        }

        close() {
            if (this._close) {
                this._close();
            }
            writableClose(this._id);
            return Promise.resolve();
        }
    }

    // TransformStream
    class TransformStream {
        constructor(transformer, writableStrategy, readableStrategy) {
            this.readable = new ReadableStream(undefined, readableStrategy);
            const readableController = this.readable._controller;

            this.writable = new WritableStream({
                write: (chunk) => {
                    if (transformer && transformer.transform) {
                        transformer.transform(chunk, {
                            enqueue: (c) => readableController.enqueue(c),
                            error: (e) => readableController.error(e),
                            terminate: () => readableController.close()
                        });
                    } else {
                        // Pass-through by default
                        readableController.enqueue(chunk);
                    }
                },
                close: () => {
                    if (transformer && transformer.flush) {
                        transformer.flush({
                            enqueue: (c) => readableController.enqueue(c),
                            error: (e) => readableController.error(e),
                            terminate: () => readableController.close()
                        });
                    }
                    readableController.close();
                }
            }, writableStrategy);

            // Call start if provided
            if (transformer && transformer.start) {
                transformer.start({
                    enqueue: (c) => readableController.enqueue(c),
                    error: (e) => readableController.error(e),
                    terminate: () => readableController.close()
                });
            }
        }
    }

    // Export
    globalThis.ReadableStream = ReadableStream;
    globalThis.WritableStream = WritableStream;
    globalThis.TransformStream = TransformStream;
    globalThis.ReadableStreamDefaultReader = ReadableStreamDefaultReader;
    globalThis.WritableStreamDefaultWriter = WritableStreamDefaultWriter;
})();
