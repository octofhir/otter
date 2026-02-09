// Node.js fs module for Otter VM
// Provides synchronous and callback-based file system operations

const fs = {
    // Sync methods
    readFileSync(path, options) {
        const encoding = typeof options === 'string' ? options : options?.encoding;
        return __fs_read_file_sync(path, encoding);
    },

    writeFileSync(path, data, options) {
        return __fs_write_file_sync(path, data, options);
    },

    existsSync(path) {
        return __fs_exists_sync(path);
    },

    statSync(path) {
        const stat = __fs_stat_sync(path);
        return {
            ...stat,
            isFile: () => stat.isFile,
            isDirectory: () => stat.isDirectory,
            isSymbolicLink: () => stat.isSymbolicLink,
        };
    },

    readdirSync(path, options) {
        return __fs_readdir_sync(path, options);
    },

    mkdirSync(path, options) {
        return __fs_mkdir_sync(path, options);
    },

    rmdirSync(path, options) {
        return __fs_rmdir_sync(path, options);
    },

    unlinkSync(path) {
        return __fs_unlink_sync(path);
    },

    // Callback methods (wrap sync for now)
    readFile(path, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = undefined;
        }
        try {
            const result = fs.readFileSync(path, options);
            queueMicrotask(() => callback(null, result));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    writeFile(path, data, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = undefined;
        }
        try {
            fs.writeFileSync(path, data, options);
            queueMicrotask(() => callback(null));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    stat(path, callback) {
        try {
            const result = fs.statSync(path);
            queueMicrotask(() => callback(null, result));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    readdir(path, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = undefined;
        }
        try {
            const result = fs.readdirSync(path, options);
            queueMicrotask(() => callback(null, result));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    mkdir(path, options, callback) {
        if (typeof options === 'function') {
            callback = options;
            options = undefined;
        }
        try {
            fs.mkdirSync(path, options);
            queueMicrotask(() => callback(null));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    unlink(path, callback) {
        try {
            fs.unlinkSync(path);
            queueMicrotask(() => callback(null));
        } catch (err) {
            queueMicrotask(() => callback(err));
        }
    },

    exists(path, callback) {
        const result = fs.existsSync(path);
        queueMicrotask(() => callback(result));
    },

    // Constants
    constants: {
        F_OK: 0,
        R_OK: 4,
        W_OK: 2,
        X_OK: 1,
    }
};

// Export for module system
export default fs;
