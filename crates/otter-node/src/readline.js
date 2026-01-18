// node:readline module implementation
// Provides an interface for reading data from a Readable stream one line at a time.

(function() {
    'use strict';

    // Get EventEmitter from node:events
    const { EventEmitter } = globalThis.__otter_node_builtins?.events || { EventEmitter: class extends Object {} };

    /**
     * Interface for reading line-by-line from a stream.
     */
    class Interface extends EventEmitter {
        constructor(options) {
            super();

            if (typeof options === 'object' && options !== null) {
                this.input = options.input || process.stdin;
                this.output = options.output || process.stdout;
                this.terminal = options.terminal !== undefined ? options.terminal : this.output.isTTY;
                this._prompt = options.prompt !== undefined ? options.prompt : '> ';
                this.historySize = options.historySize || 30;
                this.removeHistoryDuplicates = options.removeHistoryDuplicates || false;
                this.crlfDelay = options.crlfDelay || 100;
                this.escapeCodeTimeout = options.escapeCodeTimeout || 500;
                this.tabSize = options.tabSize || 8;
            } else {
                // Legacy: createInterface(input, output, completer, terminal)
                this.input = options || process.stdin;
                this.output = arguments[1] || process.stdout;
                this.terminal = arguments[3] !== undefined ? arguments[3] : this.output?.isTTY;
                this._prompt = '> ';
                this.historySize = 30;
            }

            this.line = '';
            this.cursor = 0;
            this.history = [];
            this.historyIndex = -1;
            this._closed = false;
            this._paused = false;
            this._questionCallback = null;
            this._lineBuffer = '';

            // Set up input handling
            this._setupInput();
        }

        _setupInput() {
            if (!this.input) return;

            // Handle data from input
            const onData = (data) => {
                if (this._closed) return;

                const str = typeof data === 'string' ? data : data.toString('utf8');
                this._lineBuffer += str;

                // Process complete lines
                let newlineIndex;
                while ((newlineIndex = this._lineBuffer.indexOf('\n')) !== -1) {
                    let line = this._lineBuffer.slice(0, newlineIndex);
                    this._lineBuffer = this._lineBuffer.slice(newlineIndex + 1);

                    // Handle CRLF
                    if (line.endsWith('\r')) {
                        line = line.slice(0, -1);
                    }

                    this.line = line;
                    this.cursor = line.length;

                    // If we have a question callback, call it
                    if (this._questionCallback) {
                        const callback = this._questionCallback;
                        this._questionCallback = null;
                        callback(line);
                    }

                    // Emit line event
                    this.emit('line', line);
                }
            };

            const onEnd = () => {
                if (this._closed) return;
                // Process any remaining data
                if (this._lineBuffer.length > 0) {
                    const line = this._lineBuffer;
                    this._lineBuffer = '';
                    this.emit('line', line);
                }
                this.close();
            };

            const onError = (err) => {
                this.emit('error', err);
            };

            // Try to attach listeners
            if (typeof this.input.on === 'function') {
                this.input.on('data', onData);
                this.input.on('end', onEnd);
                this.input.on('error', onError);
            } else if (typeof this.input.addEventListener === 'function') {
                // Web-style streams
                this.input.addEventListener('data', (e) => onData(e.data || e));
            }

            this._onData = onData;
            this._onEnd = onEnd;
            this._onError = onError;
        }

        /**
         * Display the prompt and wait for user input.
         */
        prompt(preserveCursor = false) {
            if (this._closed) return;

            if (this.output && typeof this.output.write === 'function') {
                this.output.write(this._prompt);
            }

            if (!preserveCursor) {
                this.cursor = this.line.length;
            }
        }

        /**
         * Set the prompt string.
         */
        setPrompt(prompt) {
            this._prompt = prompt;
        }

        /**
         * Get the current prompt string.
         */
        getPrompt() {
            return this._prompt;
        }

        /**
         * Ask a question and wait for the answer.
         */
        question(query, options, callback) {
            if (this._closed) {
                throw new Error('readline was closed');
            }

            // Handle overloaded signatures
            if (typeof options === 'function') {
                callback = options;
                options = {};
            }

            // Write the query/prompt
            if (this.output && typeof this.output.write === 'function') {
                this.output.write(query);
            }

            // Store callback to be called when line is received
            this._questionCallback = callback;
        }

        /**
         * Write data to the output stream.
         */
        write(data, key) {
            if (this._closed) return;

            if (data !== null && data !== undefined) {
                if (this.output && typeof this.output.write === 'function') {
                    this.output.write(data);
                }
            }

            // Handle key sequences (for terminal mode)
            if (key && typeof key === 'object') {
                // Could handle special keys here in the future
            }
        }

        /**
         * Pause the input stream.
         */
        pause() {
            if (this._paused) return this;
            this._paused = true;

            if (this.input && typeof this.input.pause === 'function') {
                this.input.pause();
            }

            this.emit('pause');
            return this;
        }

        /**
         * Resume the input stream.
         */
        resume() {
            if (!this._paused) return this;
            this._paused = false;

            if (this.input && typeof this.input.resume === 'function') {
                this.input.resume();
            }

            this.emit('resume');
            return this;
        }

        /**
         * Close the Interface.
         */
        close() {
            if (this._closed) return;
            this._closed = true;

            // Remove listeners
            if (this.input) {
                if (typeof this.input.removeListener === 'function') {
                    if (this._onData) this.input.removeListener('data', this._onData);
                    if (this._onEnd) this.input.removeListener('end', this._onEnd);
                    if (this._onError) this.input.removeListener('error', this._onError);
                }
            }

            // Clear any pending question
            if (this._questionCallback) {
                this._questionCallback = null;
            }

            this.emit('close');
        }

        /**
         * Get the current cursor position.
         */
        getCursorPos() {
            return {
                rows: 0,
                cols: this.cursor
            };
        }

        /**
         * Symbol.asyncIterator for async iteration support.
         */
        [Symbol.asyncIterator]() {
            const self = this;
            const lineQueue = [];
            let resolveNext = null;
            let closed = false;

            self.on('line', (line) => {
                if (resolveNext) {
                    resolveNext({ value: line, done: false });
                    resolveNext = null;
                } else {
                    lineQueue.push(line);
                }
            });

            self.on('close', () => {
                closed = true;
                if (resolveNext) {
                    resolveNext({ value: undefined, done: true });
                    resolveNext = null;
                }
            });

            return {
                next() {
                    if (lineQueue.length > 0) {
                        return Promise.resolve({ value: lineQueue.shift(), done: false });
                    }
                    if (closed) {
                        return Promise.resolve({ value: undefined, done: true });
                    }
                    return new Promise((resolve) => {
                        resolveNext = resolve;
                    });
                },
                return() {
                    self.close();
                    return Promise.resolve({ value: undefined, done: true });
                }
            };
        }
    }

    /**
     * Create a readline Interface.
     */
    function createInterface(options) {
        return new Interface(options);
    }

    /**
     * Clear the current line.
     */
    function clearLine(stream, dir, callback) {
        if (!stream || typeof stream.write !== 'function') {
            if (callback) callback();
            return false;
        }

        // ANSI escape codes for clearing line
        let sequence;
        if (dir < 0) {
            // Clear from cursor to beginning
            sequence = '\x1b[1K';
        } else if (dir > 0) {
            // Clear from cursor to end
            sequence = '\x1b[0K';
        } else {
            // Clear entire line
            sequence = '\x1b[2K';
        }

        stream.write(sequence);
        if (callback) callback();
        return true;
    }

    /**
     * Clear the screen from the current cursor position.
     */
    function clearScreenDown(stream, callback) {
        if (!stream || typeof stream.write !== 'function') {
            if (callback) callback();
            return false;
        }

        stream.write('\x1b[0J');
        if (callback) callback();
        return true;
    }

    /**
     * Move cursor to a specific position.
     */
    function cursorTo(stream, x, y, callback) {
        if (!stream || typeof stream.write !== 'function') {
            if (callback) callback();
            return false;
        }

        if (typeof y === 'function') {
            callback = y;
            y = undefined;
        }

        if (y !== undefined) {
            stream.write(`\x1b[${y + 1};${x + 1}H`);
        } else {
            stream.write(`\x1b[${x + 1}G`);
        }

        if (callback) callback();
        return true;
    }

    /**
     * Move cursor relative to current position.
     */
    function moveCursor(stream, dx, dy, callback) {
        if (!stream || typeof stream.write !== 'function') {
            if (callback) callback();
            return false;
        }

        let sequence = '';
        if (dx > 0) {
            sequence += `\x1b[${dx}C`;
        } else if (dx < 0) {
            sequence += `\x1b[${-dx}D`;
        }

        if (dy > 0) {
            sequence += `\x1b[${dy}B`;
        } else if (dy < 0) {
            sequence += `\x1b[${-dy}A`;
        }

        if (sequence) {
            stream.write(sequence);
        }

        if (callback) callback();
        return true;
    }

    /**
     * Emit keys from input stream (for terminal key handling).
     */
    function emitKeypressEvents(stream, iface) {
        // Simplified implementation - in Node.js this does complex terminal handling
        // For now, we just ensure the stream is in raw mode if possible
        if (stream && typeof stream.setRawMode === 'function') {
            // Don't automatically set raw mode - let the user do it
        }
    }

    // Create promises versions
    const promises = {
        createInterface(options) {
            const rl = createInterface(options);

            // Add promise-based question method
            rl.question = function(query, options) {
                return new Promise((resolve) => {
                    Interface.prototype.question.call(this, query, options, resolve);
                });
            };

            return rl;
        }
    };

    const readlineModule = {
        Interface,
        createInterface,
        clearLine,
        clearScreenDown,
        cursorTo,
        moveCursor,
        emitKeypressEvents,
        promises,
    };
    readlineModule.default = readlineModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('readline', readlineModule);
        globalThis.__registerModule('node:readline', readlineModule);
    }
})();
