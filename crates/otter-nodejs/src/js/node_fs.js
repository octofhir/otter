// Node.js fs module - ESM export wrapper
// This module wraps native ops to provide Node.js-compatible fs API

export function readFileSync(path, options) {
    const encoding = typeof options === 'string' ? options : options?.encoding;
    return __fs_read_file_sync(path, encoding);
}

export function writeFileSync(path, data, options) {
    return __fs_write_file_sync(path, data, options);
}

export function existsSync(path) {
    return __fs_exists_sync(path);
}

export function statSync(path) {
    const stat = __fs_stat_sync(path);
    return {
        ...stat,
        isFile: () => stat.isFile,
        isDirectory: () => stat.isDirectory,
        isSymbolicLink: () => stat.isSymbolicLink,
    };
}

export function readdirSync(path, options) {
    return __fs_readdir_sync(path, options);
}

export function mkdirSync(path, options) {
    return __fs_mkdir_sync(path, options);
}

export function rmdirSync(path, options) {
    return __fs_rmdir_sync(path, options);
}

export function unlinkSync(path) {
    return __fs_unlink_sync(path);
}

// Callback-based async wrappers
export function readFile(path, options, callback) {
    if (typeof options === 'function') {
        callback = options;
        options = undefined;
    }
    try {
        const result = readFileSync(path, options);
        queueMicrotask(() => callback(null, result));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function writeFile(path, data, options, callback) {
    if (typeof options === 'function') {
        callback = options;
        options = undefined;
    }
    try {
        writeFileSync(path, data, options);
        queueMicrotask(() => callback(null));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function stat(path, callback) {
    try {
        const result = statSync(path);
        queueMicrotask(() => callback(null, result));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function readdir(path, options, callback) {
    if (typeof options === 'function') {
        callback = options;
        options = undefined;
    }
    try {
        const result = readdirSync(path, options);
        queueMicrotask(() => callback(null, result));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function mkdir(path, options, callback) {
    if (typeof options === 'function') {
        callback = options;
        options = undefined;
    }
    try {
        mkdirSync(path, options);
        queueMicrotask(() => callback(null));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function unlink(path, callback) {
    try {
        unlinkSync(path);
        queueMicrotask(() => callback(null));
    } catch (err) {
        queueMicrotask(() => callback(err));
    }
}

export function exists(path, callback) {
    const result = existsSync(path);
    queueMicrotask(() => callback(result));
}

export const constants = {
    F_OK: 0,
    R_OK: 4,
    W_OK: 2,
    X_OK: 1,
};

// Default export for CommonJS compatibility
export default {
    readFileSync,
    writeFileSync,
    existsSync,
    statSync,
    readdirSync,
    mkdirSync,
    rmdirSync,
    unlinkSync,
    readFile,
    writeFile,
    stat,
    readdir,
    mkdir,
    unlink,
    exists,
    constants,
};
