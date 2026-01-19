/**
 * node:tty - Node.js TTY module stub
 *
 * Provides basic TTY detection functionality.
 */
(function() {
    'use strict';

    /**
     * Check if the given file descriptor refers to a TTY
     */
    function isatty(fd) {
        // In Otter, we can check if stdout/stderr are TTY using native functions if available
        // For now, assume non-TTY (safe default for CI environments)
        if (typeof globalThis.__otter_isatty === 'function') {
            return globalThis.__otter_isatty(fd);
        }
        // Default: assume not a TTY (safer for most environments)
        return false;
    }

    /**
     * ReadStream class (stub)
     */
    class ReadStream {
        constructor(fd) {
            this.fd = fd;
            this.isRaw = false;
            this.isTTY = isatty(fd);
        }

        setRawMode(mode) {
            this.isRaw = !!mode;
            return this;
        }
    }

    /**
     * WriteStream class (stub)
     */
    class WriteStream {
        constructor(fd) {
            this.fd = fd;
            this.isTTY = isatty(fd);
            this.columns = 80;
            this.rows = 24;
        }

        clearLine(dir, callback) {
            if (callback) callback();
        }

        clearScreenDown(callback) {
            if (callback) callback();
        }

        cursorTo(x, y, callback) {
            if (typeof y === 'function') {
                callback = y;
            }
            if (callback) callback();
        }

        moveCursor(dx, dy, callback) {
            if (callback) callback();
        }

        getColorDepth(env) {
            return 1;
        }

        hasColors(count, env) {
            return false;
        }

        getWindowSize() {
            return [this.columns, this.rows];
        }
    }

    // TTY module
    const ttyModule = {
        isatty,
        ReadStream,
        WriteStream,
    };

    // Add default export
    ttyModule.default = ttyModule;

    // Register module
    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('tty', ttyModule);
    }
})();
