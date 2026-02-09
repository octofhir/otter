// Node.js util module - ESM export wrapper (stub)

export function promisify(fn) {
    return function (...args) {
        return new Promise((resolve, reject) => {
            fn(...args, (err, result) => {
                if (err) reject(err);
                else resolve(result);
            });
        });
    };
}

export function callbackify(fn) {
    return function (...args) {
        const callback = args.pop();
        fn(...args)
            .then(result => callback(null, result))
            .catch(err => callback(err));
    };
}

export function deprecate(fn, msg) {
    let warned = false;
    return function (...args) {
        if (!warned) {
            console.warn(`DeprecationWarning: ${msg}`);
            warned = true;
        }
        return fn.apply(this, args);
    };
}

export function inherits(ctor, superCtor) {
    Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
}

export function format(f, ...args) {
    if (typeof f !== 'string') {
        return args.map(String).join(' ');
    }
    let i = 0;
    return f.replace(/%[sdj%]/g, (x) => {
        if (x === '%%') return '%';
        if (i >= args.length) return x;
        const arg = args[i++];
        switch (x) {
            case '%s': return String(arg);
            case '%d': return Number(arg);
            case '%j': return JSON.stringify(arg);
            default: return x;
        }
    });
}

export function inspect(obj, options) {
    return JSON.stringify(obj, null, 2);
}

export const types = {
    isArray: Array.isArray,
    isDate: (v) => v instanceof Date,
    isRegExp: (v) => v instanceof RegExp,
    isMap: (v) => v instanceof Map,
    isSet: (v) => v instanceof Set,
    isPromise: (v) => v instanceof Promise,
};

export default {
    promisify,
    callbackify,
    deprecate,
    inherits,
    format,
    inspect,
    types,
};
