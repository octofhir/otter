//! Node.js `util` module implementation (subset).
//!
//! Currently provides:
//! - `util.promisify`
//! - `util.format`
//! - `util.inspect`

/// JavaScript implementation of the `node:util` module.
pub fn util_module_js() -> &'static str {
    r#"
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

    const utilModule = {
        format,
        inspect,
        promisify,
    };
    utilModule.default = utilModule;

    if (globalThis.__registerModule) {
        globalThis.__registerModule('util', utilModule);
        globalThis.__registerModule('node:util', utilModule);
    }
})();
"#
}

