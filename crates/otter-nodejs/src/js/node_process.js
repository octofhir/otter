// Node.js process module - ESM export wrapper

export const env = new Proxy({}, {
    get(target, key) {
        if (typeof key !== 'string') return undefined;
        return __env_get(key);
    },
    has(target, key) {
        if (typeof key !== 'string') return false;
        return __env_has(key);
    },
    ownKeys() {
        return __env_keys();
    },
    getOwnPropertyDescriptor(target, key) {
        if (__env_has(key)) {
            return { configurable: true, enumerable: true, value: __env_get(key) };
        }
        return undefined;
    }
});

export function cwd() {
    return __process_cwd();
}

export function chdir(dir) {
    return __process_chdir(dir);
}

export function exit(code = 0) {
    return __process_exit(code);
}

export function hrtime(prev) {
    return __process_hrtime(prev);
}

export const pid = __process_pid();
export const platform = __process_platform();
export const arch = __process_arch();
export const version = __process_version();

export const versions = {
    otter: '0.1.0',
    node: '20.0.0'
};

export const argv = typeof __process_argv !== 'undefined' ? __process_argv() : [];

export function nextTick(callback, ...args) {
    queueMicrotask(() => callback(...args));
}

export default {
    env,
    cwd,
    chdir,
    exit,
    hrtime,
    pid,
    platform,
    arch,
    version,
    versions,
    argv,
    nextTick,
};
