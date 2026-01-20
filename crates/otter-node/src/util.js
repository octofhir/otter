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

    function formatInternal(inspectOptions, formatStr, args) {
        if (typeof formatStr !== 'string') {
            const all = [formatStr, ...args].map((v) => (typeof v === 'string' ? v : inspect(v, inspectOptions)));
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
                    return inspect(arg, inspectOptions);
                default:
                    return match;
            }
        });

        const rest = args.slice(index).map((v) => (typeof v === 'string' ? v : inspect(v, inspectOptions)));
        return rest.length ? out + ' ' + rest.join(' ') : out;
    }

    function format(formatStr, ...args) {
        return formatInternal(undefined, formatStr, args);
    }

    function formatWithOptions(inspectOptions, formatStr, ...args) {
        return formatInternal(inspectOptions, formatStr, args);
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

    const debugLoggers = Object.create(null);
    function parseDebugEnv() {
        const env = (globalThis.process && globalThis.process.env && globalThis.process.env.NODE_DEBUG) || '';
        return new Set(env
            .split(',')
            .map((token) => token.trim().toUpperCase())
            .filter(Boolean));
    }

    function debuglog(set) {
        const name = String(set).toUpperCase();
        if (Object.prototype.hasOwnProperty.call(debugLoggers, name)) {
            return debugLoggers[name];
        }
        const enabled = parseDebugEnv();
        if (!enabled.has(name)) {
            const noop = () => {};
            debugLoggers[name] = noop;
            return noop;
        }
        const logger = (...args) => {
            const prefix = `[${name}]`;
            const msg = args.map((arg) => (typeof arg === 'string' ? arg : inspect(arg))).join(' ');
            console.error(`${prefix} ${msg}`);
        };
        debugLoggers[name] = logger;
        return logger;
    }

    const NEGATIVE_NUMBER = /^-\d/;

    function parseArgs(config = {}) {
        const defaultArgs = globalThis.process && Array.isArray(globalThis.process.argv)
            ? globalThis.process.argv.slice(2)
            : [];
        const rawArgs = config.args !== undefined ? config.args : defaultArgs;
        const strict = config.strict !== undefined ? Boolean(config.strict) : true;
        const allowPositionals = config.allowPositionals !== undefined ? Boolean(config.allowPositionals) : !strict;
        const returnTokens = Boolean(config.tokens);
        const allowNegative = Boolean(config.allowNegative);
        const options = config.options !== undefined ? config.options : Object.create(null);

        validateArray(rawArgs, 'args');
        validateBoolean(strict, 'strict');
        validateBoolean(allowPositionals, 'allowPositionals');
        validateBoolean(returnTokens, 'tokens');
        validateBoolean(allowNegative, 'allowNegative');
        validateObject(options, 'options');

        const { normalizedOptions, shortToLong } = normalizeOptions(options);
        const args = rawArgs.slice();
        const tokens = argsToTokens(args, normalizedOptions, shortToLong, allowNegative);

        const result = {
            values: Object.create(null),
            positionals: [],
        };
        if (returnTokens) {
            result.tokens = tokens;
        }

        for (const token of tokens) {
            if (token.kind === 'option') {
                const definition = normalizedOptions[token.name];
                if (!definition) {
                    if (strict) {
                        throw unknownOptionError(token.rawName || token.name);
                    }
                    storeUnknownOptionValue(token, result.values);
                    continue;
                }
                if (definition.type === 'boolean' && token.value !== undefined) {
                    throw invalidOptionValueError(token.rawName || `--${token.name}`);
                }
                if (definition.type === 'string' && token.value === undefined) {
                    throw missingOptionValueError(token.rawName || `--${token.name}`);
                }
                storeDefinedOption(token, definition, result.values);
            } else {
                if (!allowPositionals) {
                    throw unexpectedPositionalError(token.value);
                }
                result.positionals.push(token.value);
            }
        }

        fillDefaults(normalizedOptions, result.values);
        return result;
    }

    function normalizeOptions(rawOptions) {
        const normalizedOptions = Object.create(null);
        const shortToLong = Object.create(null);

        for (const [longName, optionConfig] of Object.entries(rawOptions)) {
            validateObject(optionConfig, `options.${longName}`);
            const type = optionConfig.type;
            if (type !== 'string' && type !== 'boolean') {
                throw invalidArgType(`options.${longName}.type`, ['string', 'boolean'], type);
            }

            const normalized = {
                type,
                multiple: false,
                default: undefined,
            };

            if (Object.prototype.hasOwnProperty.call(optionConfig, 'short')) {
                const short = String(optionConfig.short);
                if (short.length !== 1) {
                    throw invalidArgValue(`options.${longName}.short`, short, 'must be a single character');
                }
                shortToLong[short] = longName;
                normalized.short = short;
            }

            if (Object.prototype.hasOwnProperty.call(optionConfig, 'multiple')) {
                const multiple = optionConfig.multiple;
                validateBoolean(multiple, `options.${longName}.multiple`);
                normalized.multiple = Boolean(multiple);
            }

            if (Object.prototype.hasOwnProperty.call(optionConfig, 'default')) {
                const defValue = optionConfig.default;
                validateDefaultValue(normalized, longName, defValue);
                normalized.default = normalized.multiple ? defValue.slice() : defValue;
            }

            normalizedOptions[longName] = normalized;
        }

        return { normalizedOptions, shortToLong };
    }

    function validateDefaultValue(definition, name, value) {
        if (definition.multiple) {
            if (!Array.isArray(value)) {
                throw invalidArgType(`options.${name}.default`, 'Array', value);
            }
            for (let i = 0; i < value.length; i++) {
                validateValueType(definition.type, `options.${name}.default`, value[i]);
            }
        } else {
            validateValueType(definition.type, `options.${name}.default`, value);
        }
    }

    function validateValueType(type, label, value) {
        if (type === 'string') {
            validateString(value, label);
        } else if (type === 'boolean') {
            validateBoolean(value, label);
        }
    }

    function argsToTokens(args, options, shortToLong, allowNegative) {
        const tokens = [];
        for (let i = 0; i < args.length;) {
            const arg = `${args[i]}`;
            if (arg === '--') {
                for (let j = i + 1; j < args.length; j += 1) {
                    tokens.push({ kind: 'positional', value: args[j], index: j });
                }
                break;
            }

            if (arg.startsWith('--') && arg.length > 2) {
                const { tokens: newTokens, consumed } = parseLongOption(arg, args, i, options, allowNegative);
                tokens.push(...newTokens);
                i += consumed;
                continue;
            }

            if (arg.startsWith('-') && arg.length > 1 && arg !== '-' && (!allowNegative || !NEGATIVE_NUMBER.test(arg))) {
                const { tokens: newTokens, consumed } = parseShortOption(arg, args, i, options, shortToLong, allowNegative);
                tokens.push(...newTokens);
                i += consumed;
                continue;
            }

            tokens.push({ kind: 'positional', value: arg, index: i });
            i += 1;
        }
        return tokens;
    }

    function parseLongOption(arg, args, index, options, allowNegative) {
        const eqIndex = arg.indexOf('=');
        const rawName = eqIndex === -1 ? arg : arg.slice(0, eqIndex);
        const name = rawName.slice(2);
        const definition = options[name];
        const inline = eqIndex !== -1;
        let value = inline ? arg.slice(eqIndex + 1) : undefined;
        let consumed = 1;

        if (!inline && definition && definition.type === 'string') {
            const nextArg = args[index + 1];
            if (nextArg !== undefined && !isOptionLike(nextArg, allowNegative)) {
                value = `${nextArg}`;
                consumed = 2;
            }
        }

        const token = {
            kind: 'option',
            name,
            rawName,
            value,
            inlineValue: inline ? true : undefined,
            index,
        };
        return { tokens: [token], consumed };
    }

    function parseShortOption(arg, args, index, options, shortToLong, allowNegative) {
        const tokens = [];
        let consumed = 1;
        const sequence = arg.slice(1);
        for (let i = 0; i < sequence.length; i += 1) {
            const char = sequence[i];
            const longName = shortToLong[char];
            const definition = longName ? options[longName] : undefined;
            const rawName = `-${char}`;

            if (definition && definition.type === 'string') {
                let value;
                let inline;
                const remainder = sequence.slice(i + 1);
                if (remainder.length > 0) {
                    value = remainder;
                    inline = true;
                } else {
                    const nextArg = args[index + consumed];
                    if (nextArg !== undefined && !isOptionLike(nextArg, allowNegative)) {
                        value = `${nextArg}`;
                        consumed += 1;
                    }
                }
                tokens.push({
                    kind: 'option',
                    name: longName,
                    rawName,
                    value,
                    inlineValue: inline ? true : undefined,
                    index,
                });
                break;
            }

            tokens.push({
                kind: 'option',
                name: longName || char,
                rawName,
                value: undefined,
                inlineValue: undefined,
                index,
            });
        }

        return { tokens, consumed };
    }

    function isOptionLike(value, allowNegative) {
        if (typeof value !== 'string') return false;
        if (value === '-' || value === '--') return false;
        if (allowNegative && NEGATIVE_NUMBER.test(value)) return false;
        return value.startsWith('-') && value.length > 1;
    }

    function storeDefinedOption(token, definition, values) {
        const existing = values[token.name];
        const value = definition.type === 'boolean' ? true : token.value;
        if (definition.multiple) {
            if (existing === undefined) {
                values[token.name] = [value];
            } else {
                existing.push(value);
            }
        } else {
            values[token.name] = value;
        }
    }

    function storeUnknownOptionValue(token, values) {
        const existing = values[token.name];
        const value = token.value !== undefined ? token.value : true;
        if (existing === undefined) {
            values[token.name] = value;
        } else if (Array.isArray(existing)) {
            existing.push(value);
        } else {
            values[token.name] = [existing, value];
        }
    }

    function fillDefaults(options, values) {
        Array.prototype.forEach.call(Object.keys(options), (name) => {
            if (Object.prototype.hasOwnProperty.call(values, name)) {
                return;
            }
            const definition = options[name];
            if (definition.default !== undefined) {
                values[name] = definition.multiple ? definition.default.slice() : definition.default;
            }
        });
    }

    function unexpectedPositionalError(value) {
        return createParseArgsError(
            'ERR_PARSE_ARGS_UNEXPECTED_POSITIONAL',
            `Unexpected argument '${value}'. This command does not take positional arguments`,
        );
    }

    function unknownOptionError(name) {
        return createParseArgsError(
            'ERR_PARSE_ARGS_UNKNOWN_OPTION',
            `Unknown option '${name}'. To specify a positional argument starting with a '-', place it at the end of the command after '--', as in '-- "${name}"'`,
        );
    }

    function invalidOptionValueError(name) {
        return createParseArgsError('ERR_PARSE_ARGS_INVALID_OPTION_VALUE', `Option '${name}' does not take an argument`);
    }

    function missingOptionValueError(name) {
        return createParseArgsError('ERR_PARSE_ARGS_INVALID_OPTION_VALUE', `Option '${name} <value>' argument missing`);
    }

    function validateArray(value, label) {
        if (!Array.isArray(value)) {
            throw invalidArgType(label, 'Array', value);
        }
    }

    function validateBoolean(value, label) {
        if (typeof value !== 'boolean') {
            throw invalidArgType(label, 'boolean', value);
        }
    }

    function validateString(value, label) {
        if (typeof value !== 'string') {
            throw invalidArgType(label, 'string', value);
        }
    }

    function validateObject(value, label) {
        if (!value || typeof value !== 'object') {
            throw invalidArgType(label, 'object', value);
        }
    }

    function invalidArgType(label, expected, actual) {
        const expectedDesc = Array.isArray(expected) ? expected.join('|') : expected;
        const message = `The "${label}" argument must be of type ${expectedDesc}. Received type ${typeof actual} ('${actual}')`;
        const err = new TypeError(message);
        err.code = 'ERR_INVALID_ARG_TYPE';
        return err;
    }

    function invalidArgValue(label, actual, reason) {
        const message = `The property '${label}' ${reason}. Received '${actual}'`;
        const err = new TypeError(message);
        err.code = 'ERR_INVALID_ARG_VALUE';
        return err;
    }

    function createParseArgsError(code, message) {
        const err = new TypeError(message);
        err.code = code;
        return err;
    }

    const HTTP_TOKEN_RE = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

    class MIMEParams {
        #map = new Map();

        delete(name) {
            return this.#map.delete(normalizeParamName(name));
        }

        get(name) {
            return this.#map.get(normalizeParamName(name));
        }

        has(name) {
            return this.#map.has(normalizeParamName(name));
        }

        set(name, value) {
            this.#map.set(normalizeParamName(name), String(value));
            return this;
        }

        *entries() {
            yield* this.#map.entries();
        }

        *keys() {
            yield* this.#map.keys();
        }

        *values() {
            yield* this.#map.values();
        }

        toString() {
            const parts = [];
            for (const [key, value] of this.#map.entries()) {
                parts.push(`${key}=${formatParamValue(value)}`);
            }
            return parts.join(';');
        }

        toJSON() {
            return this.toString();
        }

        [Symbol.iterator]() {
            return this.entries();
        }
    }

    class MIMEType {
        #type;
        #subtype;
        #parameters;

        constructor(input) {
            const raw = `${input}`.trim();
            const { type, subtype, params } = parseMimeType(raw);
            this.#type = type;
            this.#subtype = subtype;
            this.#parameters = params;
        }

        get type() {
            return this.#type;
        }

        set type(value) {
            const normalized = normalizeMimeToken(`${value}`, 'type');
            this.#type = normalized;
        }

        get subtype() {
            return this.#subtype;
        }

        set subtype(value) {
            const normalized = normalizeMimeToken(`${value}`, 'subtype');
            this.#subtype = normalized;
        }

        get essence() {
            return `${this.#type}/${this.#subtype}`;
        }

        get params() {
            return this.#parameters;
        }

        toString() {
            const base = `${this.#type}/${this.#subtype}`;
            const params = this.#parameters.toString();
            return params.length ? `${base};${params}` : base;
        }
    }

    function parseMimeType(input) {
        if (!input) {
            throw mimeSyntaxError('type', input, Math.max(0, input.length));
        }
        const slashIndex = input.indexOf('/');
        if (slashIndex === -1) {
            throw mimeSyntaxError('type', input, 0);
        }
        const typePart = input.slice(0, slashIndex).trim();
        const remainder = input.slice(slashIndex + 1);
        const semicolonIndex = remainder.indexOf(';');
        const subtypePart = (semicolonIndex === -1
            ? remainder
            : remainder.slice(0, semicolonIndex)).trim();
        const paramsPart = semicolonIndex === -1 ? '' : remainder.slice(semicolonIndex + 1);

        if (typePart.length === 0) {
            throw mimeSyntaxError('type', input, 0);
        }
        if (subtypePart.length === 0) {
            throw mimeSyntaxError('subtype', input, slashIndex + 1);
        }

        const type = normalizeMimeToken(typePart, 'type');
        const subtype = normalizeMimeToken(subtypePart, 'subtype');
        const params = instantiateMimeParams(paramsPart);

        return { type, subtype, params };
    }

    function normalizeMimeToken(value, component) {
        const normalized = `${value}`.trim();
        if (normalized.length === 0) {
            throw mimeSyntaxError(component, value, 0);
        }
        for (let i = 0; i < normalized.length; i += 1) {
            const char = normalized[i];
            if (!HTTP_TOKEN_RE.test(char)) {
                throw mimeSyntaxError(component, normalized, i);
            }
        }
        return normalized.toLowerCase();
    }

    function instantiateMimeParams(raw) {
        const params = new MIMEParams();
        let index = 0;
        while (index < raw.length) {
            while (index < raw.length && (raw[index] === ';' || isMimeWhitespace(raw[index]))) {
                index += 1;
            }
            if (index >= raw.length) break;

            const nameStart = index;
            while (index < raw.length && HTTP_TOKEN_RE.test(raw[index])) {
                index += 1;
            }
            const name = raw.slice(nameStart, index);
            if (name.length === 0) {
                throw mimeSyntaxError('parameter', raw, index);
            }
            while (index < raw.length && isMimeWhitespace(raw[index])) {
                index += 1;
            }
            if (raw[index] !== '=') {
                throw mimeSyntaxError('parameter', raw, index);
            }
            index += 1;
            while (index < raw.length && isMimeWhitespace(raw[index])) {
                index += 1;
            }

            let value = '';
            if (raw[index] === '"') {
                index += 1;
                let closed = false;
                while (index < raw.length) {
                    const char = raw[index];
                    if (char === '"') {
                        index += 1;
                        closed = true;
                        break;
                    }
                    if (char === '\\' && index + 1 < raw.length) {
                        index += 1;
                        value += raw[index];
                        index += 1;
                        continue;
                    }
                    value += char;
                    index += 1;
                }
                if (!closed) {
                    throw mimeSyntaxError('parameter', raw, raw.length);
                }
            } else {
                const start = index;
                while (index < raw.length && raw[index] !== ';') {
                    index += 1;
                }
                value = raw.slice(start, index).trim();
            }

            params.set(name, value);
        }
        return params;
    }

    function mimeSyntaxError(component, source, position) {
        const err = new TypeError(`The MIME syntax for a ${component} in "${source}" is invalid at ${position}`);
        err.code = 'ERR_INVALID_MIME_SYNTAX';
        return err;
    }

    function isMimeWhitespace(ch) {
        return ch === ' ' || ch === '\t';
    }

    function normalizeParamName(name) {
        const normalized = `${name}`.trim().toLowerCase();
        if (normalized.length === 0 || !HTTP_TOKEN_RE.test(normalized)) {
            throw mimeSyntaxError('parameter', name, 0);
        }
        return normalized;
    }

    function formatParamValue(value) {
        if (value === '') return '';
        if (/[;=]/.test(value) || value.includes('"') || value.includes('\\') || /\s/.test(value)) {
            return `"${value.replace(/(["\\])/g, '\\$1')}"`;
        }
        return value;
    }

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
    function getTag(value) {
        if (value && typeof value === 'object' && Symbol.toStringTag in value) {
            return value[Symbol.toStringTag];
        }
        return Object.prototype.toString.call(value).slice(8, -1);
    }

    function isBoxed(value) {
        const tag = getTag(value);
        return ['String', 'Number', 'Boolean', 'Symbol', 'BigInt'].includes(tag);
    }

    const types = {
        isArray: Array.isArray,
        isArrayBuffer: (v) => typeof ArrayBuffer !== 'undefined' && v instanceof ArrayBuffer,
        isAnyArrayBuffer: (v) =>
            (typeof ArrayBuffer !== 'undefined' && v instanceof ArrayBuffer) ||
            (typeof SharedArrayBuffer !== 'undefined' && v instanceof SharedArrayBuffer),
        isSharedArrayBuffer: (v) => typeof SharedArrayBuffer !== 'undefined' && v instanceof SharedArrayBuffer,
        isArrayBufferView: (v) => ArrayBuffer.isView(v),
        isArgumentsObject: (v) => getTag(v) === 'Arguments',
        isBooleanObject: (v) => isBoxed(v) && typeof v.valueOf() === 'boolean',
        isNumberObject: (v) => isBoxed(v) && typeof v.valueOf() === 'number',
        isStringObject: (v) => isBoxed(v) && typeof v.valueOf() === 'string',
        isSymbolObject: (v) => isBoxed(v) && typeof v.valueOf() === 'symbol',
        isBigIntObject: (v) => isBoxed(v) && typeof v.valueOf() === 'bigint',
        isDate: (v) => v instanceof Date,
        isRegExp: (v) => v instanceof RegExp,
        isMap: (v) => v instanceof Map,
        isSet: (v) => v instanceof Set,
        isWeakMap: (v) => v instanceof WeakMap,
        isWeakSet: (v) => v instanceof WeakSet,
        isMapIterator: (v) => getTag(v) === 'Map Iterator',
        isSetIterator: (v) => getTag(v) === 'Set Iterator',
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
        isNativeError: (v) => v instanceof Error || (v && typeof v === 'object' && typeof v.name === 'string' && typeof v.message === 'string'),
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
        formatWithOptions,
        inspect,
        promisify,
        inherits,
        deprecate,
        debuglog,
        parseArgs,
        types,
        callbackify,
        isDeepStrictEqual,
        MIMEType,
        MIMEParams,
    };
    utilModule.default = utilModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('util', utilModule);
    }
})();
