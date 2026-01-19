// node:util module implementation
// Provides util.promisify, util.format, util.inspect

(function() {
    'use strict';

    const kInspectCustom = Symbol.for('nodejs.util.inspect.custom');
    const kPromisifyCustom = Symbol.for('nodejs.util.promisify.custom');

    function isPlainObject(value) {
        if (!value || typeof value !== 'object') return false;
        const proto = Object.getPrototypeOf(value);
        return proto === Object.prototype || proto === null;
    }

    function quoteString(str) {
        const escaped = String(str)
            .replaceAll('\\', '\\\\')
            .replaceAll('\n', '\\n')
            .replaceAll('\r', '\\r')
            .replaceAll('\t', '\\t')
            .replaceAll('"', '\\"');
        return '"' + escaped + '"';
    }

    function inspect(value, options) {
        const opts = options && typeof options === 'object' ? options : {};
        const depth = Number.isFinite(opts.depth) ? opts.depth : 2;

        const seen = new WeakSet();

        function inner(v, d) {
            if (v === null) return 'null';
            if (v === undefined) return 'undefined';

            const t = typeof v;
            if (t === 'string') return quoteString(v);
            if (t === 'number') return Object.is(v, -0) ? '-0' : String(v);
            if (t === 'boolean') return v ? 'true' : 'false';
            if (t === 'bigint') return String(v) + 'n';
            if (t === 'symbol') return v.toString();
            if (t === 'function') return v.name ? `[Function: ${v.name}]` : '[Function]';

            // Objects
            try {
                const custom = v && v[kInspectCustom];
                if (typeof custom === 'function') {
                    const res = custom.call(v, d, opts, inspect);
                    if (typeof res === 'string') return res;
                }
            } catch (_) {
                // ignore custom inspect errors
            }

            if (v instanceof Date) return v.toISOString();
            if (v instanceof RegExp) return v.toString();
            if (v instanceof Error) return v.stack || v.toString();

            if (seen.has(v)) return '[Circular]';
            seen.add(v);

            if (Array.isArray(v)) {
                if (d < 0) return '[Array]';
                const max = Number.isFinite(opts.maxArrayLength) ? opts.maxArrayLength : 100;
                const len = v.length >>> 0;
                const items = [];
                const limit = Math.min(len, max);
                for (let i = 0; i < limit; i += 1) {
                    items.push(inner(v[i], d - 1));
                }
                if (len > limit) items.push(`... ${len - limit} more items`);
                return `[ ${items.join(', ')} ]`;
            }

            if (d < 0) return '[Object]';

            const keys = Object.keys(v);
            const parts = [];
            for (const key of keys) {
                let val;
                try {
                    val = v[key];
                } catch (e) {
                    val = e;
                }
                parts.push(`${key}: ${inner(val, d - 1)}`);
            }

            if (isPlainObject(v)) return `{ ${parts.join(', ')} }`;
            const ctor = v && v.constructor && v.constructor.name ? v.constructor.name : 'Object';
            return `${ctor} { ${parts.join(', ')} }`;
        }

        return inner(value, depth);
    }

    inspect.custom = kInspectCustom;
    inspect.defaultOptions = { depth: 2, colors: false, showHidden: false, maxArrayLength: 100 };

    function format(formatStr, ...args) {
        if (typeof formatStr !== 'string') {
            const all = [formatStr, ...args].map((v) => (typeof v === 'string' ? v : inspect(v)));
            return all.join(' ');
        }

        let index = 0;
        const out = formatStr.replace(/%([sdifoOj%])/g, (match, code) => {
            if (code === '%') return '%';
            const arg = args[index++];
            switch (code) {
                case 's': return String(arg);
                case 'd': return Number(arg).toString();
                case 'i': return parseInt(arg, 10).toString();
                case 'f': return parseFloat(arg).toString();
                case 'j':
                    try { return JSON.stringify(arg); } catch (_) { return '[Circular]'; }
                case 'o':
                case 'O':
                    return inspect(arg);
                default:
                    return match;
            }
        });

        const rest = args.slice(index).map((v) => (typeof v === 'string' ? v : inspect(v)));
        return rest.length ? out + ' ' + rest.join(' ') : out;
    }

    function promisify(original) {
        if (typeof original !== 'function') {
            throw new TypeError('The "original" argument must be of type function');
        }

        const custom = original[kPromisifyCustom];
        if (typeof custom === 'function') return custom;

        return function promisified(...args) {
            return new Promise((resolve, reject) => {
                original.call(this, ...args, (err, ...values) => {
                    if (err) {
                        reject(err);
                        return;
                    }
                    if (values.length <= 1) resolve(values[0]);
                    else resolve(values);
                });
            });
        };
    }

    promisify.custom = kPromisifyCustom;

    // Node.js util.inherits - sets up prototype chain inheritance
    function inherits(ctor, superCtor) {
        if (ctor === undefined || ctor === null) {
            throw new TypeError('The constructor to "inherits" must not be null or undefined');
        }
        if (superCtor === undefined || superCtor === null) {
            throw new TypeError('The super constructor to "inherits" must not be null or undefined');
        }
        if (superCtor.prototype === undefined) {
            throw new TypeError('The super constructor to "inherits" must have a prototype');
        }
        Object.defineProperty(ctor, 'super_', {
            value: superCtor,
            writable: true,
            configurable: true
        });
        Object.setPrototypeOf(ctor.prototype, superCtor.prototype);
    }

    // Node.js util.deprecate - wraps a function with deprecation warning
    function deprecate(fn, msg, code) {
        if (typeof fn !== 'function') {
            throw new TypeError('The "fn" argument must be of type function');
        }
        let warned = false;
        function deprecated(...args) {
            if (!warned) {
                warned = true;
                console.warn(`DeprecationWarning: ${msg}${code ? ` [${code}]` : ''}`);
            }
            return fn.apply(this, args);
        }
        return deprecated;
    }

    // Node.js util.types - type checking utilities
    const types = {
        isArray: Array.isArray,
        isArrayBuffer: (v) => v instanceof ArrayBuffer,
        isDate: (v) => v instanceof Date,
        isRegExp: (v) => v instanceof RegExp,
        isMap: (v) => v instanceof Map,
        isSet: (v) => v instanceof Set,
        isWeakMap: (v) => v instanceof WeakMap,
        isWeakSet: (v) => v instanceof WeakSet,
        isPromise: (v) => v instanceof Promise,
        isGeneratorFunction: (v) => v && v.constructor && v.constructor.name === 'GeneratorFunction',
        isAsyncFunction: (v) => v && v.constructor && v.constructor.name === 'AsyncFunction',
        isTypedArray: (v) => ArrayBuffer.isView(v) && !(v instanceof DataView),
        isDataView: (v) => v instanceof DataView,
        isUint8Array: (v) => v instanceof Uint8Array,
        isUint16Array: (v) => v instanceof Uint16Array,
        isUint32Array: (v) => v instanceof Uint32Array,
        isInt8Array: (v) => v instanceof Int8Array,
        isInt16Array: (v) => v instanceof Int16Array,
        isInt32Array: (v) => v instanceof Int32Array,
        isFloat32Array: (v) => v instanceof Float32Array,
        isFloat64Array: (v) => v instanceof Float64Array,
        isBigInt64Array: (v) => typeof BigInt64Array !== 'undefined' && v instanceof BigInt64Array,
        isBigUint64Array: (v) => typeof BigUint64Array !== 'undefined' && v instanceof BigUint64Array,
    };

    // Node.js util.callbackify - converts promise-returning function to callback style
    function callbackify(original) {
        if (typeof original !== 'function') {
            throw new TypeError('The "original" argument must be of type function');
        }
        return function callbackified(...args) {
            const callback = args.pop();
            if (typeof callback !== 'function') {
                throw new TypeError('The last argument must be of type function');
            }
            Promise.resolve(original.apply(this, args))
                .then((result) => callback(null, result))
                .catch((err) => callback(err));
        };
    }

    // Node.js util.isDeepStrictEqual - simplified version
    function isDeepStrictEqual(a, b) {
        if (Object.is(a, b)) return true;
        if (typeof a !== typeof b) return false;
        if (typeof a !== 'object' || a === null || b === null) return false;

        if (Array.isArray(a) !== Array.isArray(b)) return false;
        if (Array.isArray(a)) {
            if (a.length !== b.length) return false;
            for (let i = 0; i < a.length; i++) {
                if (!isDeepStrictEqual(a[i], b[i])) return false;
            }
            return true;
        }

        const keysA = Object.keys(a);
        const keysB = Object.keys(b);
        if (keysA.length !== keysB.length) return false;
        for (const key of keysA) {
            if (!Object.prototype.hasOwnProperty.call(b, key)) return false;
            if (!isDeepStrictEqual(a[key], b[key])) return false;
        }
        return true;
    }

    const utilModule = {
        format,
        inspect,
        promisify,
        inherits,
        deprecate,
        types,
        callbackify,
        isDeepStrictEqual,
    };
    utilModule.default = utilModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('util', utilModule);
    }
})();
