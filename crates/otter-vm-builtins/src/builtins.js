// Well-known symbols (must be defined early for use in prototypes)
const _symbolIterator = __Symbol_getWellKnown('iterator');
const _symbolAsyncIterator = __Symbol_getWellKnown('asyncIterator');
const _symbolToStringTag = __Symbol_getWellKnown('toStringTag');
const _symbolHasInstance = __Symbol_getWellKnown('hasInstance');
const _symbolToPrimitive = __Symbol_getWellKnown('toPrimitive');
const _symbolIsConcatSpreadable = __Symbol_getWellKnown('isConcatSpreadable');
const _symbolMatch = __Symbol_getWellKnown('match');
const _symbolMatchAll = __Symbol_getWellKnown('matchAll');
const _symbolReplace = __Symbol_getWellKnown('replace');
const _symbolSearch = __Symbol_getWellKnown('search');
const _symbolSplit = __Symbol_getWellKnown('split');
const _symbolSpecies = __Symbol_getWellKnown('species');
const _symbolUnscopables = __Symbol_getWellKnown('unscopables');

// Reflect built-in (must be defined EARLY for use in Object methods)
globalThis.Reflect = {};
globalThis.Reflect.getOwnPropertyDescriptor = function (target, propertyKey) { return __Reflect_getOwnPropertyDescriptor(target, propertyKey); };
globalThis.Reflect.getPrototypeOf = function (target) { return __Reflect_getPrototypeOf(target); };
globalThis.Reflect.setPrototypeOf = function (target, prototype) { return __Reflect_setPrototypeOf(target, prototype); };
globalThis.Reflect.get = function (target, propertyKey, receiver) { return __Reflect_get(target, propertyKey, receiver); };
globalThis.Reflect.set = function (target, propertyKey, value, receiver) { return __Reflect_set(target, propertyKey, value, receiver); };
globalThis.Reflect.has = function (target, propertyKey) { return __Reflect_has(target, propertyKey); };
globalThis.Reflect.deleteProperty = function (target, propertyKey) { return __Reflect_deleteProperty(target, propertyKey); };
globalThis.Reflect.ownKeys = function (target) { return __Reflect_ownKeys(target); };
globalThis.Reflect.defineProperty = function (target, propertyKey, attributes) { return __Reflect_defineProperty(target, propertyKey, attributes); };
globalThis.Reflect.isExtensible = function (target) { return __Reflect_isExtensible(target); };
globalThis.Reflect.preventExtensions = function (target) { return __Reflect_preventExtensions(target); };
globalThis.Reflect.apply = function (target, thisArgument, argumentsList) {
    if (typeof target !== 'function') {
        throw new TypeError('Reflect.apply requires target to be a function');
    }
    if (!Array.isArray(argumentsList)) {
        throw new TypeError('Reflect.apply requires argumentsList to be an array');
    }
    return target.apply(thisArgument, argumentsList);
};
globalThis.Reflect.construct = function (target, argumentsList, newTarget) {
    if (typeof target !== 'function') {
        throw new TypeError('Reflect.construct requires target to be a constructor');
    }
    if (!Array.isArray(argumentsList)) {
        throw new TypeError('Reflect.construct requires argumentsList to be an array');
    }
    if (newTarget === undefined) {
        newTarget = target;
    }
    if (typeof newTarget !== 'function') {
        throw new TypeError('Reflect.construct requires newTarget to be a constructor');
    }
    if (target.__non_constructor === true) {
        throw new TypeError('Reflect.construct requires target to be a constructor');
    }
    if (newTarget.__non_constructor === true) {
        throw new TypeError('Reflect.construct requires newTarget to be a constructor');
    }
    return new target(...argumentsList);
};



// eval built-in (global eval only)
globalThis.eval = function (code) {
    if (typeof code !== 'string') {
        return code;
    }

    const result = __otter_eval(code);
    if (result && result.ok === false) {
        const message = result.message || '';
        switch (result.errorType) {
            case 'SyntaxError':
                throw new SyntaxError(message);
            case 'ReferenceError':
                throw new ReferenceError(message);
            case 'RangeError':
                throw new RangeError(message);
            case 'TypeError':
                throw new TypeError(message);
            default:
                throw new Error(message);
        }
    }
    return result ? result.value : undefined;
};


function __markNonConstructor(fn) {
    if (typeof fn !== 'function') {
        return fn;
    }
    try {
        const Obj = globalThis.Object || Object;
        if (typeof Obj === 'undefined') {
        } else if (typeof Obj.defineProperty !== 'function') {
        }
        Obj.defineProperty(fn, "__non_constructor", {
            value: true,
            writable: false,
            enumerable: false,
            configurable: false,
        });
    } catch (e) {
    }
    return fn;
}

// Object built-in wrapper
// Object built-in wrapper
// Try to set a test property on 'this'
const testFunc = function() { return 42; };
this.TestProperty123 = testFunc;
globalThis.Object = function (value) {
    if (value === undefined || value === null) {
        return {};
    }
    if (typeof value === 'object' || typeof value === 'function') {
        return value;
    }
    // TODO: Coerce primitives to objects
    return value;
};

globalThis.Object.keys = function (obj) {
    return __native_Object_keys(obj);
};
globalThis.Object.values = function (obj) {
    return __Object_values(obj);
};
globalThis.Object.entries = function (obj) {
    return __Object_entries(obj);
};
globalThis.Object.assign = function (target, ...sources) {
    return __Object_assign(target, ...sources);
};
globalThis.Object.hasOwn = function (obj, key) {
    // Use Object.getOwnPropertyDescriptor which properly handles functions
    if (typeof obj === 'object' && obj !== null || typeof obj === 'function') {
        return globalThis.Object.getOwnPropertyDescriptor(obj, key) !== undefined;
    }
    return false;
};
// Object mutability methods (native ops)
globalThis.Object.freeze = function (obj) {
    return __Object_freeze(obj);
};
globalThis.Object.isFrozen = function (obj) {
    return __Object_isFrozen(obj);
};
globalThis.Object.seal = function (obj) {
    return __Object_seal(obj);
};
globalThis.Object.isSealed = function (obj) {
    return __Object_isSealed(obj);
};
globalThis.Object.preventExtensions = function (obj) {
    return __Object_preventExtensions(obj);
};
globalThis.Object.isExtensible = function (obj) {
    return __Object_isExtensible(obj);
};
// Object.defineProperty(obj, prop, descriptor)
const _defineProperty = function (obj, prop, descriptor) {
    if (obj === null || (typeof obj !== 'object' && typeof obj !== 'function')) {
        throw new TypeError('Object.defineProperty called on non-object');
    }
    if (descriptor === null || typeof descriptor !== 'object') {
        throw new TypeError('Property description must be an object');
    }
    return __Object_defineProperty(obj, prop, descriptor);
};
globalThis.Object.defineProperty = _defineProperty;
Object.defineProperty = _defineProperty;


// Object.defineProperties(obj, props)
globalThis.Object.defineProperties = function (obj, props) {
    if (obj === null || (typeof obj !== 'object' && typeof obj !== 'function')) {
        throw new TypeError('Object.defineProperties called on non-object');
    }
    const keys = Object.keys(props);
    for (const key of keys) {
        // Use globalThis.Object.defineProperty to ensure we call the function we just defined
        globalThis.Object.defineProperty(obj, key, props[key]);
    }
    return obj;
};
// Object.create(proto, propertiesObject?)
globalThis.Object.create = function (proto, propertiesObject) {
    if (proto !== null && typeof proto !== 'object' && typeof proto !== 'function') {
        throw new TypeError('Object prototype may only be an Object or null');
    }
    return __Object_create(proto, propertiesObject);
};
Object.create = globalThis.Object.create;
// Object.is(value1, value2) - SameValue algorithm
globalThis.Object.is = function (value1, value2) {
    return __Object_is(value1, value2);
};
// Object.fromEntries(iterable)
globalThis.Object.fromEntries = function (iterable) {
    if (iterable === null || iterable === undefined) {
        throw new TypeError('Object.fromEntries requires an iterable argument');
    }
    const obj = {};
    for (const entry of iterable) {
        if (entry === null || typeof entry !== 'object') {
            throw new TypeError('Iterator value is not an entry object');
        }
        const key = entry[0];
        const value = entry[1];
        obj[key] = value;
    }
    return obj;
};
// Object.getOwnPropertyNames(obj)
globalThis.Object.getOwnPropertyNames = function (obj) {
    return __Object_getOwnPropertyNames(obj);
};
// Object.getOwnPropertySymbols(obj)
globalThis.Object.getOwnPropertySymbols = function (obj) {
    return __Object_getOwnPropertySymbols(obj);
};
globalThis.Object.getOwnPropertyDescriptor = function (obj, prop) {
    if (obj === null || (typeof obj !== 'object' && typeof obj !== 'function')) {
        throw new TypeError('Object.getOwnPropertyDescriptor called on non-object');
    }
    return __Reflect_getOwnPropertyDescriptor(obj, prop);
};
// Object.getOwnPropertyDescriptors(obj)
globalThis.Object.getOwnPropertyDescriptors = function (obj) {
    if (obj === null || (typeof obj !== 'object' && typeof obj !== 'function')) {
        throw new TypeError('Object.getOwnPropertyDescriptors called on non-object');
    }
    return __Object_getOwnPropertyDescriptors(obj);
};
// Object.getPrototypeOf(obj)
globalThis.Object.getPrototypeOf = function (obj) {
    if (obj === null || obj === undefined) {
        throw new TypeError('Cannot convert undefined or null to object');
    }
    // Convert primitives to objects
    if (typeof obj !== 'object' && typeof obj !== 'function') {
        obj = Object(obj);
    }
    return Reflect.getPrototypeOf(obj);
};
// Object.setPrototypeOf(obj, proto)
globalThis.Object.setPrototypeOf = function (obj, proto) {
    if (obj === null || obj === undefined) {
        throw new TypeError('Object.setPrototypeOf called on null or undefined');
    }
    if (proto !== null && typeof proto !== 'object') {
        throw new TypeError('Object prototype may only be an Object or null');
    }
    if (typeof obj !== 'object' && typeof obj !== 'function') {
        return obj; // Per spec, return obj unchanged for primitives
    }
    Reflect.setPrototypeOf(obj, proto);
    return obj;
};


// Object.prototype methods
const _objectPrototype = {
    hasOwnProperty: function (prop) {
        return Object.hasOwn(this, prop);
    },
    isPrototypeOf: function (obj) {
        if (obj === null || obj === undefined) {
            return false;
        }
        let proto = Reflect.getPrototypeOf(obj);
        while (proto !== null) {
            if (proto === this) {
                return true;
            }
            proto = Reflect.getPrototypeOf(proto);
        }
        return false;
    },
    propertyIsEnumerable: function (prop) {
        const desc = Reflect.getOwnPropertyDescriptor(this, prop);
        return desc !== undefined && desc.enumerable === true;
    },
    valueOf: function () {
        return this;
    },
    toLocaleString: function () {
        if (this === null || this === undefined) {
            throw new TypeError('Cannot call toLocaleString on null or undefined');
        }
        return String(this);
    },
};

globalThis.Object.prototype = _objectPrototype;


// Array built-in wrapper
globalThis.Array = function (...args) {
    if (args.length === 1 && typeof args[0] === 'number') {
        const len = args[0];
        // Create array of given length
        const arr = [];
        arr.length = len;
        return arr;
    }
    return [...args];
};

Array.isArray = function (val) {
    return __Array_isArray(val);
};
Array.from = function (arrayLike) {
    return __Array_from(arrayLike);
};
Array.of = function (...items) {
    return __Array_of(items);
};


// Math built-in
globalThis.Math = {
    // Constants
    E: 2.718281828459045,
    LN10: 2.302585092994046,
    LN2: 0.6931471805599453,
    LOG10E: 0.4342944819032518,
    LOG2E: 1.4426950408889634,
    PI: 3.141592653589793,
    SQRT1_2: 0.7071067811865476,
    SQRT2: 1.4142135623730951,

    // Basic
    abs: function (x) { return __Math_abs(x); },
    ceil: function (x) { return __Math_ceil(x); },
    floor: function (x) { return __Math_floor(x); },
    round: function (x) { return __Math_round(x); },
    trunc: function (x) { return __Math_trunc(x); },
    sign: function (x) { return __Math_sign(x); },

    // Roots and Powers
    sqrt: function (x) { return __Math_sqrt(x); },
    cbrt: function (x) { return __Math_cbrt(x); },
    pow: function (base, exp) { return __Math_pow(base, exp); },
    hypot: function (...values) { return __Math_hypot(...values); },

    // Exponentials and Logarithms
    exp: function (x) { return __Math_exp(x); },
    expm1: function (x) { return __Math_expm1(x); },
    log: function (x) { return __Math_log(x); },
    log1p: function (x) { return __Math_log1p(x); },
    log2: function (x) { return __Math_log2(x); },
    log10: function (x) { return __Math_log10(x); },

    // Trigonometry
    sin: function (x) { return __Math_sin(x); },
    cos: function (x) { return __Math_cos(x); },
    tan: function (x) { return __Math_tan(x); },
    asin: function (x) { return __Math_asin(x); },
    acos: function (x) { return __Math_acos(x); },
    atan: function (x) { return __Math_atan(x); },
    atan2: function (y, x) { return __Math_atan2(y, x); },

    // Hyperbolic
    sinh: function (x) { return __Math_sinh(x); },
    cosh: function (x) { return __Math_cosh(x); },
    tanh: function (x) { return __Math_tanh(x); },
    asinh: function (x) { return __Math_asinh(x); },
    acosh: function (x) { return __Math_acosh(x); },
    atanh: function (x) { return __Math_atanh(x); },

    // Min/Max/Random
    min: function (...values) { return __Math_min(...values); },
    max: function (...values) { return __Math_max(...values); },
    random: function () { return __Math_random(); },

    // Special
    clz32: function (x) { return __Math_clz32(x); },
    imul: function (a, b) { return __Math_imul(a, b); },
    fround: function (x) { return __Math_fround(x); },
    f16round: function (x) { return __Math_f16round(x); },
};


// JSON built-in
globalThis.JSON = {
    parse: function (text, reviver) {
        const result = __JSON_parse(text);
        if (reviver === undefined) {
            return result;
        }
        // With reviver, apply it to each value
        return JSON.applyReviver(result, '', reviver);
    },

    stringify: function (value, replacer, space) {
        // Convert value to JSON string representation for native code
        const jsonStr = JSON.stringifyValue(value, replacer);
        if (jsonStr === undefined) {
            return undefined;
        }
        // Call native stringify with space parameter
        return __JSON_stringify(jsonStr, replacer, space);
    },

    // ES2024+ JSON.rawJSON(string)
    rawJSON: function (string) {
        const result = __JSON_rawJSON(String(string));
        return JSON.parseValue(result);
    },

    // ES2024+ JSON.isRawJSON(value)
    isRawJSON: function (value) {
        if (value === null || typeof value !== 'object') {
            return false;
        }
        return value.__isRawJSON__ === true;
    },

    // Helper: Parse JSON string to JS value (recursive)
    parseValue: function (str) {
        if (str === 'null') return null;
        if (str === 'true') return true;
        if (str === 'false') return false;
        if (str === 'undefined') return undefined;

        // Try number
        if (/^-?\d+(\.\d+)?([eE][+-]?\d+)?$/.test(str)) {
            return Number(str);
        }

        // Try string (quoted)
        if (str.startsWith('"') && str.endsWith('"')) {
            return JSON.parseString(str);
        }

        // Try array
        if (str.startsWith('[') && str.endsWith(']')) {
            return JSON.parseArray(str);
        }

        // Try object
        if (str.startsWith('{') && str.endsWith('}')) {
            return JSON.parseObject(str);
        }

        return str;
    },

    // Helper: Parse JSON string literal
    parseString: function (str) {
        // Remove quotes and unescape
        let result = '';
        let i = 1; // Skip opening quote
        const len = str.length - 1; // Skip closing quote
        while (i < len) {
            const c = str[i];
            if (c === '\\' && i + 1 < len) {
                const next = str[i + 1];
                if (next === '"') { result += '"'; i += 2; }
                else if (next === '\\') { result += '\\'; i += 2; }
                else if (next === '/') { result += '/'; i += 2; }
                else if (next === 'b') { result += '\b'; i += 2; }
                else if (next === 'f') { result += '\f'; i += 2; }
                else if (next === 'n') { result += '\n'; i += 2; }
                else if (next === 'r') { result += '\r'; i += 2; }
                else if (next === 't') { result += '\t'; i += 2; }
                else if (next === 'u' && i + 5 < len) {
                    const hex = str.slice(i + 2, i + 6);
                    result += String.fromCharCode(parseInt(hex, 16));
                    i += 6;
                } else {
                    result += c;
                    i++;
                }
            } else {
                result += c;
                i++;
            }
        }
        return result;
    },

    // Helper: Parse JSON array
    parseArray: function (str) {
        const result = [];
        const inner = str.slice(1, -1).trim();
        if (inner === '') return result;

        let depth = 0;
        let inString = false;
        let escaped = false;
        let start = 0;

        for (let i = 0; i <= inner.length; i++) {
            const c = inner[i];

            if (escaped) {
                escaped = false;
                continue;
            }

            if (c === '\\' && inString) {
                escaped = true;
                continue;
            }

            if (c === '"' && !escaped) {
                inString = !inString;
                continue;
            }

            if (!inString) {
                if (c === '[' || c === '{') depth++;
                else if (c === ']' || c === '}') depth--;
                else if ((c === ',' || i === inner.length) && depth === 0) {
                    const value = inner.slice(start, i).trim();
                    if (value !== '') {
                        result.push(JSON.parseValue(value));
                    }
                    start = i + 1;
                }
            }
        }

        return result;
    },

    // Helper: Parse JSON object
    parseObject: function (str) {
        const result = {};
        const inner = str.slice(1, -1).trim();
        if (inner === '') return result;

        let depth = 0;
        let inString = false;
        let escaped = false;
        let start = 0;
        let key = null;
        let colonPos = -1;

        for (let i = 0; i <= inner.length; i++) {
            const c = inner[i];

            if (escaped) {
                escaped = false;
                continue;
            }

            if (c === '\\' && inString) {
                escaped = true;
                continue;
            }

            if (c === '"' && !escaped) {
                inString = !inString;
                continue;
            }

            if (!inString) {
                if (c === '[' || c === '{') depth++;
                else if (c === ']' || c === '}') depth--;
                else if (c === ':' && depth === 0 && key === null) {
                    const keyStr = inner.slice(start, i).trim();
                    key = JSON.parseValue(keyStr);
                    colonPos = i;
                    start = i + 1;
                } else if ((c === ',' || i === inner.length) && depth === 0) {
                    if (key !== null) {
                        const valueStr = inner.slice(colonPos + 1, i).trim();
                        result[key] = JSON.parseValue(valueStr);
                        key = null;
                        colonPos = -1;
                    }
                    start = i + 1;
                }
            }
        }

        return result;
    },

    // Helper: Stringify value to JSON string
    stringifyValue: function (value, replacer) {
        if (value === undefined) return undefined;
        if (value === null) return 'null';
        if (typeof value === 'boolean') return value ? 'true' : 'false';
        if (typeof value === 'number') {
            if (!isFinite(value)) return 'null';
            return String(value);
        }
        if (typeof value === 'string') {
            return JSON.escapeString(value);
        }
        if (typeof value === 'function') return undefined;
        if (typeof value === 'symbol') return undefined;

        // Check for rawJSON
        if (value && value.__isRawJSON__) {
            return value.rawJSON;
        }

        // Check for toJSON method
        if (value && typeof value.toJSON === 'function') {
            return JSON.stringifyValue(value.toJSON());
        }

        // Array
        if (Array.isArray(value)) {
            const parts = [];
            for (let i = 0; i < value.length; i++) {
                const v = JSON.stringifyValue(value[i], replacer);
                parts.push(v === undefined ? 'null' : v);
            }
            return '[' + parts.join(',') + ']';
        }

        // Object
        if (typeof value === 'object') {
            const parts = [];
            const keys = Object.keys(value);
            for (let i = 0; i < keys.length; i++) {
                const key = keys[i];
                if (typeof key !== 'string') continue;
                const propVal = value[key];
                const v = JSON.stringifyValue(propVal, replacer);
                if (v !== undefined) {
                    parts.push(JSON.escapeString(key) + ':' + v);
                }
            }
            return '{' + parts.join(',') + '}';
        }

        return undefined;
    },

    // Helper: Escape string for JSON
    escapeString: function (str) {
        let result = '"';
        for (let i = 0; i < str.length; i++) {
            const c = str.charCodeAt(i);
            if (c === 0x22) result += '\\"';      // "
            else if (c === 0x5C) result += '\\\\'; // \
            else if (c === 0x08) result += '\\b';  // backspace
            else if (c === 0x0C) result += '\\f';  // formfeed
            else if (c === 0x0A) result += '\\n';  // newline
            else if (c === 0x0D) result += '\\r';  // carriage return
            else if (c === 0x09) result += '\\t';  // tab
            else if (c < 0x20) result += '\\u' + c.toString(16).padStart(4, '0');
            else result += str[i];
        }
        return result + '"';
    },

    // Helper: Apply reviver to parsed value
    applyReviver: function (value, key, reviver) {
        if (value !== null && typeof value === 'object') {
            if (Array.isArray(value)) {
                for (let i = 0; i < value.length; i++) {
                    value[i] = JSON.applyReviver(value[i], String(i), reviver);
                }
            } else {
                const keys = Object.keys(value);
                for (let i = 0; i < keys.length; i++) {
                    const k = keys[i];
                    value[k] = JSON.applyReviver(value[k], k, reviver);
                }
            }
        }
        return reviver.call({ [key]: value }, key, value);
    },
};


// String built-in wrapper
globalThis.String = function (value) {
    // Avoid recursion: this function replaces the global `String`.
    // Use JS ToString-like semantics instead of calling `String(...)`.
    if (value === undefined) return '';
    if (value === null) return 'null';
    if (typeof value === 'symbol') {
        throw new TypeError('Cannot convert a Symbol value to a string');
    }
    return '' + value;
};

String.fromCharCode = function (...codes) {
    return __String_fromCharCode(...codes);
};

String.fromCodePoint = function (...codePoints) {
    return __String_fromCodePoint(...codePoints);
};

// String.prototype methods
String.prototype = {
    charAt: function (index) {
        return __String_charAt(this, index);
    },
    charCodeAt: function (index) {
        return __String_charCodeAt(this, index);
    },
    codePointAt: function (pos) {
        return __String_codePointAt(this, pos);
    },
    concat: function (...strings) {
        return __String_concat(this, ...strings);
    },
    includes: function (searchString, position) {
        return __String_includes(this, searchString, position);
    },
    indexOf: function (searchValue, fromIndex) {
        return __String_indexOf(this, searchValue, fromIndex);
    },
    lastIndexOf: function (searchValue, fromIndex) {
        return __String_lastIndexOf(this, searchValue, fromIndex);
    },
    match: function (regexp) {
        if (regexp !== null && regexp !== undefined && regexp[_symbolMatch] !== undefined) {
            return regexp[_symbolMatch](this);
        }
        const re = RegExp(regexp);
        return re[_symbolMatch](this);
    },
    matchAll: function (regexp) {
        if (regexp !== null && regexp !== undefined && regexp[_symbolMatchAll] !== undefined) {
            return regexp[_symbolMatchAll](this);
        }
        const re = RegExp(regexp);
        return re[_symbolMatchAll](this);
    },
    search: function (regexp) {
        if (regexp !== null && regexp !== undefined && regexp[_symbolSearch] !== undefined) {
            return regexp[_symbolSearch](this);
        }
        const re = RegExp(regexp);
        return re[_symbolSearch](this);
    },
    slice: function (start, end) {
        return __String_slice(this, start, end);
    },
    substring: function (start, end) {
        return __String_substring(this, start, end);
    },
    split: function (separator, limit) {
        return __String_split(this, separator, limit);
    },
    toLowerCase: function () {
        return __String_toLowerCase(this);
    },
    toUpperCase: function () {
        return __String_toUpperCase(this);
    },
    toLocaleLowerCase: function (locales) {
        return __String_toLocaleLowerCase(this, locales);
    },
    toLocaleUpperCase: function (locales) {
        return __String_toLocaleUpperCase(this, locales);
    },
    trim: function () {
        return __String_trim(this);
    },
    trimStart: function () {
        return __String_trimStart(this);
    },
    trimEnd: function () {
        return __String_trimEnd(this);
    },
    trimLeft: function () {
        // Alias for trimStart
        return __String_trimStart(this);
    },
    trimRight: function () {
        // Alias for trimEnd
        return __String_trimEnd(this);
    },
    replace: function (searchValue, replaceValue) {
        return __String_replace(this, searchValue, replaceValue);
    },
    replaceAll: function (searchValue, replaceValue) {
        return __String_replaceAll(this, searchValue, replaceValue);
    },
    startsWith: function (searchString, position) {
        return __String_startsWith(this, searchString, position);
    },
    endsWith: function (searchString, endPosition) {
        return __String_endsWith(this, searchString, endPosition);
    },
    repeat: function (count) {
        return __String_repeat(this, count);
    },
    padStart: function (targetLength, padString) {
        return __String_padStart(this, targetLength, padString);
    },
    padEnd: function (targetLength, padString) {
        return __String_padEnd(this, targetLength, padString);
    },
    at: function (index) {
        return __String_at(this, index);
    },
    normalize: function (form) {
        return __String_normalize(this, form);
    },
    isWellFormed: function () {
        return __String_isWellFormed(this);
    },
    toWellFormed: function () {
        return __String_toWellFormed(this);
    },
    localeCompare: function (compareString, locales, options) {
        return __String_localeCompare(this, compareString, locales, options);
    },
    get length() {
        return __String_length(this);
    },
    toString: function () {
        return this;
    },
    valueOf: function () {
        return this;
    },

    // Symbol.iterator - enables for-of loops over strings
    [_symbolIterator]: function () {
        const str = String(this);
        let index = 0;
        return {
            next: () => {
                if (index < str.length) {
                    // Handle surrogate pairs for proper Unicode iteration
                    const codePoint = str.codePointAt(index);
                    const char = String.fromCodePoint(codePoint);
                    index += char.length;
                    return { value: char, done: false };
                }
                return { value: undefined, done: true };
            },
            [_symbolIterator]: function () { return this; }
        };
    },
};


// Number built-in wrapper
globalThis.Number = function (value) {
    if (value === undefined) return 0;
    return +value; // Coerce to number
};

// Number static constants
Number.EPSILON = 2.220446049250313e-16;
Number.MAX_SAFE_INTEGER = 9007199254740991;
Number.MIN_SAFE_INTEGER = -9007199254740991;
Number.MAX_VALUE = 1.7976931348623157e+308;
Number.MIN_VALUE = 5e-324;
Number.NaN = NaN;
Number.POSITIVE_INFINITY = Infinity;
Number.NEGATIVE_INFINITY = -Infinity;

// Number static methods
Number.isFinite = function (value) {
    return __Number_isFinite(value);
};

Number.isInteger = function (value) {
    return __Number_isInteger(value);
};

Number.isNaN = function (value) {
    return __Number_isNaN(value);
};

Number.isSafeInteger = function (value) {
    return __Number_isSafeInteger(value);
};

Number.parseFloat = function (string) {
    return __Number_parseFloat(string);
};

Number.parseInt = function (string, radix) {
    return __Number_parseInt(string, radix);
};

// Number.prototype methods
Number.prototype = {
    toFixed: function (digits) {
        return __Number_toFixed(this, digits);
    },
    toExponential: function (fractionDigits) {
        return __Number_toExponential(this, fractionDigits);
    },
    toPrecision: function (precision) {
        return __Number_toPrecision(this, precision);
    },
    toString: function (radix) {
        return __Number_toString(this, radix);
    },
    toLocaleString: function (locales, options) {
        return __Number_toLocaleString(this, locales, options);
    },
    valueOf: function () {
        return __Number_valueOf(this);
    },
};


// Boolean built-in wrapper
globalThis.Boolean = function (value) {
    // ToBoolean conversion
    if (value === undefined || value === null) return false;
    if (typeof value === 'boolean') return value;
    if (typeof value === 'number') return value !== 0 && !Number.isNaN(value);
    if (typeof value === 'string') return value.length > 0;
    return true; // Objects are truthy
};

// Boolean.prototype methods
Boolean.prototype = {
    valueOf: function () {
        return __Boolean_valueOf(this);
    },
    toString: function () {
        return __Boolean_toString(this);
    },
};


// RegExp built-in
globalThis.RegExp = function (pattern, flags) {
    // Handle RegExp argument
    if (pattern && typeof pattern === 'object' && pattern._pattern !== undefined) {
        if (flags === undefined) {
            flags = pattern._flags;
        }
        pattern = pattern._pattern;
    }

    const _pattern = pattern === undefined ? '' : String(pattern);
    const _flags = flags === undefined ? '' : String(flags);

    const re = Object.create(RegExp.prototype);
    re._pattern = _pattern;
    re._flags = _flags;
    Object.defineProperty(re, "lastIndex", {
        value: 0,
        writable: true,
        enumerable: false,
        configurable: false,
    });
    return re;
};

RegExp.prototype = {
    get source() { return __RegExp_source(this._pattern); },
    get flags() { return __RegExp_flags(this._pattern, this._flags); },
    get global() { return __RegExp_global(this._pattern, this._flags); },
    get ignoreCase() { return __RegExp_ignoreCase(this._pattern, this._flags); },
    get multiline() { return __RegExp_multiline(this._pattern, this._flags); },
    get dotAll() { return __RegExp_dotAll(this._pattern, this._flags); },
    get sticky() { return __RegExp_sticky(this._pattern, this._flags); },
    get unicode() { return __RegExp_unicode(this._pattern, this._flags); },
    get unicodeSets() { return __RegExp_unicodeSets(this._pattern, this._flags); },
    get hasIndices() { return __RegExp_hasIndices(this._pattern, this._flags); },
    test: function (string) {
        return __RegExp_test(this._pattern, this._flags, string);
    },
    exec: function (string) {
        const result = __RegExp_exec(this._pattern, this._flags, string);
        if (result === null) return null;
        const parsed = JSON.parse(result);
        const arr = parsed.matches;
        arr.index = parsed.index;
        arr.input = parsed.input;
        arr.groups = undefined; // TODO: named capture groups
        return arr;
    },
    toString: function () {
        return __RegExp_toString(this._pattern, this._flags);
    },
    // Symbol.match
    [_symbolMatch]: function (string) {
        const result = __RegExp_match(this._pattern, this._flags, string);
        if (result === null) return null;
        if (typeof result === 'string') {
            // Global match returns JSON array of strings
            if (result.startsWith('[')) {
                return JSON.parse(result);
            }
            // Non-global match returns exec-style result
            const parsed = JSON.parse(result);
            const arr = parsed.matches;
            arr.index = parsed.index;
            arr.input = parsed.input;
            arr.groups = undefined;
            return arr;
        }
        return result;
    },
    // Symbol.matchAll
    [_symbolMatchAll]: function (string) {
        const result = __RegExp_matchAll(this._pattern, this._flags, string);
        const matches = JSON.parse(result);
        let index = 0;
        return {
            next: function () {
                if (index >= matches.length) {
                    return { done: true, value: undefined };
                }
                const m = matches[index++];
                const arr = m.matches;
                arr.index = m.index;
                arr.groups = undefined;
                return { done: false, value: arr };
            },
            [_symbolIterator]: function () { return this; }
        };
    },
    // Symbol.replace
    [_symbolReplace]: function (string, replacement) {
        return __RegExp_replace(this._pattern, this._flags, string, replacement);
    },
    // Symbol.search
    [_symbolSearch]: function (string) {
        return __RegExp_search(this._pattern, this._flags, string);
    },
    // Symbol.split
    [_symbolSplit]: function (string, limit) {
        const result = __RegExp_split(this._pattern, this._flags, string, limit);
        return JSON.parse(result);
    },
};

RegExp.prototype.constructor = RegExp;

// RegExp.escape (ES2026)
RegExp.escape = function (string) {
    return __RegExp_escape(string);
};


// Array.prototype methods (simplified - real impl needs prototype chain)
Array.prototype = {
    // === Mutating Methods ===
    push: function (...items) {
        return __Array_push_native(this, items);
    },
    pop: function () {
        return __Array_pop(this);
    },
    shift: function () {
        return __Array_shift(this);
    },
    unshift: function (...items) {
        return __Array_unshift(this, items);
    },
    splice: function (start, deleteCount, ...items) {
        return __Array_splice({
            arr: this,
            start: start,
            delete_count: deleteCount,
            items: items.length > 0 ? items : null
        });
    },
    reverse: function () {
        return __Array_reverse(this);
    },
    sort: function (compareFn) {
        // Note: compareFn not supported in JSON ops - lexicographic sort only
        return __Array_sort(this);
    },
    fill: function (value, start, end) {
        return __Array_fill({
            arr: this,
            value: value,
            start: start,
            end: end
        });
    },
    copyWithin: function (target, start, end) {
        return __Array_copyWithin({
            arr: this,
            target: target,
            start: start,
            end: end
        });
    },

    // === Non-Mutating Methods ===
    slice: function (start, end) {
        return __Array_slice({ arr: this, start: start, end: end });
    },
    concat: function (...items) {
        return __Array_concat(this, items);
    },
    flat: function (depth) {
        return __Array_flat({ arr: this, depth: depth });
    },
    flatMap: function (callback, thisArg) {
        // Execute callback in JS, pass results to native
        const mapped = [];
        for (let i = 0; i < this.length; i++) {
            mapped.push(callback.call(thisArg, this[i], i, this));
        }
        return __Array_flatMap({ arr: this, mapped: mapped });
    },

    // === Search Methods ===
    indexOf: function (searchElement, fromIndex) {
        return __Array_indexOf(this, searchElement);
    },
    lastIndexOf: function (searchElement, fromIndex) {
        return __Array_lastIndexOf(this, searchElement);
    },
    includes: function (searchElement, fromIndex) {
        return __Array_includes(this, searchElement);
    },
    find: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_find({ arr: this, results: results });
    },
    findIndex: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findIndex({ arr: this, results: results });
    },
    findLast: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findLast({ arr: this, results: results });
    },
    findLastIndex: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findLastIndex({ arr: this, results: results });
    },
    at: function (index) {
        return __Array_at(this, index);
    },

    // === Iteration Methods ===
    forEach: function (callback, thisArg) {
        for (let i = 0; i < this.length; i++) {
            callback.call(thisArg, this[i], i, this);
        }
        return __Array_forEach(this);
    },
    map: function (callback, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(callback.call(thisArg, this[i], i, this));
        }
        return __Array_map({ results: results });
    },
    filter: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_filter({ arr: this, results: results });
    },
    reduce: function (callback, initialValue) {
        let accumulator = initialValue !== undefined ? initialValue : this[0];
        const startIndex = initialValue !== undefined ? 0 : 1;
        for (let i = startIndex; i < this.length; i++) {
            accumulator = callback(accumulator, this[i], i, this);
        }
        return __Array_reduce({ result: accumulator });
    },
    reduceRight: function (callback, initialValue) {
        let accumulator = initialValue !== undefined ? initialValue : this[this.length - 1];
        const startIndex = initialValue !== undefined ? this.length - 1 : this.length - 2;
        for (let i = startIndex; i >= 0; i--) {
            accumulator = callback(accumulator, this[i], i, this);
        }
        return __Array_reduceRight({ result: accumulator });
    },
    every: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_every(results);
    },
    some: function (predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_some(results);
    },

    // === Conversion Methods ===
    join: function (separator) {
        return __Array_join(this, separator);
    },
    toString: function () {
        return __Array_toString(this);
    },
    get length() {
        return __Array_length(this);
    },

    // === ES2023 Immutable Methods ===
    toReversed: function () {
        return __Array_toReversed(this);
    },
    toSorted: function (compareFn) {
        // Note: compareFn not supported in JSON ops - lexicographic sort only
        return __Array_toSorted(this);
    },
    toSpliced: function (start, deleteCount, ...items) {
        return __Array_toSpliced({
            arr: this,
            start: start,
            delete_count: deleteCount,
            items: items.length > 0 ? items : null
        });
    },
    with: function (index, value) {
        return __Array_with({
            arr: this,
            index: index,
            value: value
        });
    },

    // Symbol.iterator - enables for-of loops
    [_symbolIterator]: function () {
        // NOTE: The VM currently does not support upvalue capture in nested functions.
        // Keep iterators stateful via `this` instead of closing over locals.
        return {
            _arr: this,
            _index: 0,
            next: function () {
                const i = this._index;
                const arr = this._arr;
                if (i < arr.length) {
                    this._index = i + 1;
                    return { value: arr[i], done: false };
                }
                return { value: undefined, done: true };
            },
            [_symbolIterator]: function () { return this; }
        };
    },
};


// Date built-in
globalThis.Date = function (year, month, date, hours, minutes, seconds, ms) {
    // Internal timestamp storage
    let _timestamp;

    if (arguments.length === 0) {
        _timestamp = __Date_now();
    } else if (arguments.length === 1) {
        if (typeof year === 'string') {
            _timestamp = __Date_parse(year);
        } else {
            _timestamp = +year;
        }
    } else {
        _timestamp = __Date_UTC(
            year,
            month || 0,
            date !== undefined ? date : 1,
            hours || 0,
            minutes || 0,
            seconds || 0,
            ms || 0
        );
        // Adjust for local timezone
        _timestamp -= new Date().getTimezoneOffset() * 60000;
    }

    return {
        _timestamp,
        getFullYear: function () { return __Date_getFullYear(this._timestamp); },
        getMonth: function () { return __Date_getMonth(this._timestamp); },
        getDate: function () { return __Date_getDate(this._timestamp); },
        getDay: function () { return __Date_getDay(this._timestamp); },
        getHours: function () { return __Date_getHours(this._timestamp); },
        getMinutes: function () { return __Date_getMinutes(this._timestamp); },
        getSeconds: function () { return __Date_getSeconds(this._timestamp); },
        getMilliseconds: function () { return __Date_getMilliseconds(this._timestamp); },
        getUTCFullYear: function () { return __Date_getUTCFullYear(this._timestamp); },
        getUTCMonth: function () { return __Date_getUTCMonth(this._timestamp); },
        getUTCDate: function () { return __Date_getUTCDate(this._timestamp); },
        getUTCDay: function () { return __Date_getUTCDay(this._timestamp); },
        getUTCHours: function () { return __Date_getUTCHours(this._timestamp); },
        getUTCMinutes: function () { return __Date_getUTCMinutes(this._timestamp); },
        getUTCSeconds: function () { return __Date_getUTCSeconds(this._timestamp); },
        getUTCMilliseconds: function () { return __Date_getUTCMilliseconds(this._timestamp); },
        getTime: function () { return __Date_getTime(this._timestamp); },
        getTimezoneOffset: function () { return __Date_getTimezoneOffset(); },
        setFullYear: function (y, m, d) { this._timestamp = __Date_setFullYear(this._timestamp, y, m, d); return this._timestamp; },
        setMonth: function (m, d) { this._timestamp = __Date_setMonth(this._timestamp, m, d); return this._timestamp; },
        setDate: function (d) { this._timestamp = __Date_setDate(this._timestamp, d); return this._timestamp; },
        setHours: function (h, m, s, ms) { this._timestamp = __Date_setHours(this._timestamp, h, m, s, ms); return this._timestamp; },
        setMinutes: function (m, s, ms) { this._timestamp = __Date_setMinutes(this._timestamp, m, s, ms); return this._timestamp; },
        setSeconds: function (s, ms) { this._timestamp = __Date_setSeconds(this._timestamp, s, ms); return this._timestamp; },
        setMilliseconds: function (ms) { this._timestamp = __Date_setMilliseconds(this._timestamp, ms); return this._timestamp; },
        setTime: function (t) { this._timestamp = __Date_setTime(this._timestamp, t); return this._timestamp; },
        setUTCFullYear: function (y, m, d) { this._timestamp = __Date_setUTCFullYear(this._timestamp, y, m, d); return this._timestamp; },
        setUTCMonth: function (m, d) { this._timestamp = __Date_setUTCMonth(this._timestamp, m, d); return this._timestamp; },
        setUTCDate: function (d) { this._timestamp = __Date_setUTCDate(this._timestamp, d); return this._timestamp; },
        setUTCHours: function (h, m, s, ms) { this._timestamp = __Date_setUTCHours(this._timestamp, h, m, s, ms); return this._timestamp; },
        setUTCMinutes: function (m, s, ms) { this._timestamp = __Date_setUTCMinutes(this._timestamp, m, s, ms); return this._timestamp; },
        setUTCSeconds: function (s, ms) { this._timestamp = __Date_setUTCSeconds(this._timestamp, s, ms); return this._timestamp; },
        setUTCMilliseconds: function (ms) { this._timestamp = __Date_setUTCMilliseconds(this._timestamp, ms); return this._timestamp; },
        toString: function () { return __Date_toString(this._timestamp); },
        toDateString: function () { return __Date_toDateString(this._timestamp); },
        toTimeString: function () { return __Date_toTimeString(this._timestamp); },
        toISOString: function () { return __Date_toISOString(this._timestamp); },
        toUTCString: function () { return __Date_toUTCString(this._timestamp); },
        toJSON: function () { return __Date_toJSON(this._timestamp); },
        toLocaleDateString: function () { return __Date_toLocaleDateString(this._timestamp); },
        toLocaleTimeString: function () { return __Date_toLocaleTimeString(this._timestamp); },
        toLocaleString: function () { return __Date_toLocaleString(this._timestamp); },
        valueOf: function () { return __Date_valueOf(this._timestamp); },
    };
};

Date.now = function () { return __Date_now(); };
Date.parse = function (s) { return __Date_parse(s); };
Date.UTC = function (y, m, d, h, min, s, ms) { return __Date_UTC(y, m, d, h, min, s, ms); };


// Temporal API (Stage 3, Chrome 144+ / Firefox 139+)
globalThis.Temporal = {
    Now: {
        instant: function () { return Temporal.Instant.fromEpochNanoseconds(__Temporal_Now_instant()); },
        timeZoneId: function () { return __Temporal_Now_timeZoneId(); },
        zonedDateTimeISO: function (tz) { return Temporal.ZonedDateTime.from(__Temporal_Now_zonedDateTimeISO(tz)); },
        plainDateTimeISO: function (tz) { return Temporal.PlainDateTime.from(__Temporal_Now_plainDateTimeISO(tz)); },
        plainDateISO: function (tz) { return Temporal.PlainDate.from(__Temporal_Now_plainDateISO(tz)); },
        plainTimeISO: function (tz) { return Temporal.PlainTime.from(__Temporal_Now_plainTimeISO(tz)); },
    },

    Instant: {
        from: function (thing) {
            const s = __Temporal_Instant_from(String(thing));
            return Temporal.Instant._create(s);
        },
        fromEpochSeconds: function (secs) {
            const s = __Temporal_Instant_fromEpochSeconds(secs);
            return Temporal.Instant._create(s);
        },
        fromEpochMilliseconds: function (ms) {
            const s = __Temporal_Instant_fromEpochMilliseconds(ms);
            return Temporal.Instant._create(s);
        },
        fromEpochMicroseconds: function (us) {
            const s = __Temporal_Instant_fromEpochMicroseconds(String(us));
            return Temporal.Instant._create(s);
        },
        fromEpochNanoseconds: function (ns) {
            const s = __Temporal_Instant_fromEpochNanoseconds(String(ns));
            return Temporal.Instant._create(s);
        },
        compare: function (one, two) {
            return one.epochNanoseconds === two.epochNanoseconds ? 0 :
                BigInt(one.epochNanoseconds) < BigInt(two.epochNanoseconds) ? -1 : 1;
        },
        _create: function (epochNanoseconds) {
            return {
                _ns: epochNanoseconds,
                get epochSeconds() { return __Temporal_Instant_epochSeconds(this._ns); },
                get epochMilliseconds() { return __Temporal_Instant_epochMilliseconds(this._ns); },
                get epochMicroseconds() { return __Temporal_Instant_epochMicroseconds(this._ns); },
                get epochNanoseconds() { return __Temporal_Instant_epochNanoseconds(this._ns); },
                add: function (d) { return Temporal.Instant._create(__Temporal_Instant_add(this._ns, d.total('nanoseconds'))); },
                subtract: function (d) { return Temporal.Instant._create(__Temporal_Instant_subtract(this._ns, d.total('nanoseconds'))); },
                until: function (other) { return Temporal.Duration.from('PT' + (__Temporal_Instant_until(this._ns, other._ns) / 1e9) + 'S'); },
                since: function (other) { return Temporal.Duration.from('PT' + (__Temporal_Instant_since(this._ns, other._ns) / 1e9) + 'S'); },
                round: function (opts) { return Temporal.Instant._create(__Temporal_Instant_round(this._ns, opts?.smallestUnit || opts)); },
                equals: function (other) { return __Temporal_Instant_equals(this._ns, other._ns); },
                toString: function () { return __Temporal_Instant_toString(this._ns); },
                toJSON: function () { return __Temporal_Instant_toJSON(this._ns); },
                valueOf: function () { throw new TypeError('Temporal.Instant cannot be converted to primitive'); },
                toZonedDateTimeISO: function (tz) { return Temporal.ZonedDateTime.from(__Temporal_Instant_toZonedDateTimeISO(this._ns, tz)); },
            };
        },
    },

    PlainDate: {
        from: function (thing) {
            const s = __Temporal_PlainDate_from(String(thing));
            return Temporal.PlainDate._create(s);
        },
        compare: function (one, two) {
            return __Temporal_PlainDate_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get year() { return __Temporal_PlainDate_year(this._s); },
                get month() { return __Temporal_PlainDate_month(this._s); },
                get monthCode() { return __Temporal_PlainDate_monthCode(this._s); },
                get day() { return __Temporal_PlainDate_day(this._s); },
                get dayOfWeek() { return __Temporal_PlainDate_dayOfWeek(this._s); },
                get dayOfYear() { return __Temporal_PlainDate_dayOfYear(this._s); },
                get weekOfYear() { return __Temporal_PlainDate_weekOfYear(this._s); },
                get daysInMonth() { return __Temporal_PlainDate_daysInMonth(this._s); },
                get daysInYear() { return __Temporal_PlainDate_daysInYear(this._s); },
                get monthsInYear() { return __Temporal_PlainDate_monthsInYear(this._s); },
                get inLeapYear() { return __Temporal_PlainDate_inLeapYear(this._s); },
                add: function (d) { return Temporal.PlainDate._create(__Temporal_PlainDate_add(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                subtract: function (d) { return Temporal.PlainDate._create(__Temporal_PlainDate_subtract(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                until: function (other) { return { days: __Temporal_PlainDate_until(this._s, other._s) }; },
                since: function (other) { return { days: __Temporal_PlainDate_since(this._s, other._s) }; },
                with: function (fields) { return Temporal.PlainDate._create(__Temporal_PlainDate_with(this._s, fields?.year, fields?.month, fields?.day)); },
                equals: function (other) { return __Temporal_PlainDate_equals(this._s, other._s); },
                toString: function () { return __Temporal_PlainDate_toString(this._s); },
                toJSON: function () { return __Temporal_PlainDate_toJSON(this._s); },
                toPlainDateTime: function (t) { return Temporal.PlainDateTime.from(__Temporal_PlainDate_toPlainDateTime(this._s, t?._s || t)); },
                toPlainYearMonth: function () { return Temporal.PlainYearMonth.from(__Temporal_PlainDate_toPlainYearMonth(this._s)); },
                toPlainMonthDay: function () { return Temporal.PlainMonthDay.from(__Temporal_PlainDate_toPlainMonthDay(this._s)); },
            };
        },
    },

    PlainTime: {
        from: function (thing) {
            const s = __Temporal_PlainTime_from(String(thing));
            return Temporal.PlainTime._create(s);
        },
        compare: function (one, two) {
            return __Temporal_PlainTime_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get hour() { return __Temporal_PlainTime_hour(this._s); },
                get minute() { return __Temporal_PlainTime_minute(this._s); },
                get second() { return __Temporal_PlainTime_second(this._s); },
                get millisecond() { return __Temporal_PlainTime_millisecond(this._s); },
                get microsecond() { return __Temporal_PlainTime_microsecond(this._s); },
                get nanosecond() { return __Temporal_PlainTime_nanosecond(this._s); },
                add: function (d) { return Temporal.PlainTime._create(__Temporal_PlainTime_add(this._s, d)); },
                subtract: function (d) { return Temporal.PlainTime._create(__Temporal_PlainTime_subtract(this._s, d)); },
                until: function (other) { return { nanoseconds: __Temporal_PlainTime_until(this._s, other._s) }; },
                since: function (other) { return { nanoseconds: __Temporal_PlainTime_since(this._s, other._s) }; },
                with: function (fields) { return Temporal.PlainTime._create(__Temporal_PlainTime_with(this._s, fields?.hour, fields?.minute, fields?.second, fields?.nanosecond)); },
                round: function (opts) { return Temporal.PlainTime._create(__Temporal_PlainTime_round(this._s, opts?.smallestUnit || opts)); },
                equals: function (other) { return __Temporal_PlainTime_equals(this._s, other._s); },
                toString: function () { return __Temporal_PlainTime_toString(this._s); },
                toJSON: function () { return __Temporal_PlainTime_toJSON(this._s); },
                toPlainDateTime: function (d) { return Temporal.PlainDateTime.from(__Temporal_PlainTime_toPlainDateTime(this._s, d?._s || d)); },
            };
        },
    },

    PlainDateTime: {
        from: function (thing) {
            const s = __Temporal_PlainDateTime_from(String(thing));
            return Temporal.PlainDateTime._create(s);
        },
        compare: function (one, two) {
            return __Temporal_PlainDateTime_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get year() { return __Temporal_PlainDateTime_year(this._s); },
                get month() { return __Temporal_PlainDateTime_month(this._s); },
                get day() { return __Temporal_PlainDateTime_day(this._s); },
                get hour() { return __Temporal_PlainDateTime_hour(this._s); },
                get minute() { return __Temporal_PlainDateTime_minute(this._s); },
                get second() { return __Temporal_PlainDateTime_second(this._s); },
                get millisecond() { return __Temporal_PlainDateTime_millisecond(this._s); },
                add: function (d) { return Temporal.PlainDateTime._create(__Temporal_PlainDateTime_add(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                subtract: function (d) { return Temporal.PlainDateTime._create(__Temporal_PlainDateTime_subtract(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                with: function (fields) { return Temporal.PlainDateTime._create(__Temporal_PlainDateTime_with(this._s, fields?.year, fields?.month, fields?.day, fields?.hour, fields?.minute, fields?.second)); },
                equals: function (other) { return __Temporal_PlainDateTime_equals(this._s, other._s); },
                toString: function () { return __Temporal_PlainDateTime_toString(this._s); },
                toJSON: function () { return __Temporal_PlainDateTime_toJSON(this._s); },
                toPlainDate: function () { return Temporal.PlainDate.from(__Temporal_PlainDateTime_toPlainDate(this._s)); },
                toPlainTime: function () { return Temporal.PlainTime.from(__Temporal_PlainDateTime_toPlainTime(this._s)); },
                toZonedDateTime: function (tz) { return Temporal.ZonedDateTime.from(__Temporal_PlainDateTime_toZonedDateTime(this._s, tz)); },
            };
        },
    },

    PlainYearMonth: {
        from: function (thing) {
            const s = __Temporal_PlainYearMonth_from(String(thing));
            return Temporal.PlainYearMonth._create(s);
        },
        compare: function (one, two) {
            return __Temporal_PlainYearMonth_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get year() { return __Temporal_PlainYearMonth_year(this._s); },
                get month() { return __Temporal_PlainYearMonth_month(this._s); },
                get monthCode() { return __Temporal_PlainYearMonth_monthCode(this._s); },
                get daysInMonth() { return __Temporal_PlainYearMonth_daysInMonth(this._s); },
                get daysInYear() { return __Temporal_PlainYearMonth_daysInYear(this._s); },
                get monthsInYear() { return __Temporal_PlainYearMonth_monthsInYear(this._s); },
                get inLeapYear() { return __Temporal_PlainYearMonth_inLeapYear(this._s); },
                add: function (d) { return Temporal.PlainYearMonth._create(__Temporal_PlainYearMonth_add(this._s, typeof d === 'object' ? d.months || 0 : d)); },
                subtract: function (d) { return Temporal.PlainYearMonth._create(__Temporal_PlainYearMonth_subtract(this._s, typeof d === 'object' ? d.months || 0 : d)); },
                equals: function (other) { return __Temporal_PlainYearMonth_equals(this._s, other._s); },
                toString: function () { return __Temporal_PlainYearMonth_toString(this._s); },
                toJSON: function () { return __Temporal_PlainYearMonth_toJSON(this._s); },
                toPlainDate: function (day) { return Temporal.PlainDate.from(__Temporal_PlainYearMonth_toPlainDate(this._s, day?.day || day)); },
            };
        },
    },

    PlainMonthDay: {
        from: function (thing) {
            const s = __Temporal_PlainMonthDay_from(String(thing));
            return Temporal.PlainMonthDay._create(s);
        },
        _create: function (s) {
            return {
                _s: s,
                get month() { return __Temporal_PlainMonthDay_month(this._s); },
                get monthCode() { return __Temporal_PlainMonthDay_monthCode(this._s); },
                get day() { return __Temporal_PlainMonthDay_day(this._s); },
                equals: function (other) { return __Temporal_PlainMonthDay_equals(this._s, other._s); },
                toString: function () { return __Temporal_PlainMonthDay_toString(this._s); },
                toJSON: function () { return __Temporal_PlainMonthDay_toJSON(this._s); },
                toPlainDate: function (year) { return Temporal.PlainDate.from(__Temporal_PlainMonthDay_toPlainDate(this._s, year?.year || year)); },
            };
        },
    },

    ZonedDateTime: {
        from: function (thing) {
            const s = __Temporal_ZonedDateTime_from(String(thing));
            return Temporal.ZonedDateTime._create(s);
        },
        compare: function (one, two) {
            return __Temporal_ZonedDateTime_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get year() { return __Temporal_ZonedDateTime_year(this._s); },
                get month() { return __Temporal_ZonedDateTime_month(this._s); },
                get day() { return __Temporal_ZonedDateTime_day(this._s); },
                get hour() { return __Temporal_ZonedDateTime_hour(this._s); },
                get minute() { return __Temporal_ZonedDateTime_minute(this._s); },
                get second() { return __Temporal_ZonedDateTime_second(this._s); },
                get millisecond() { return __Temporal_ZonedDateTime_millisecond(this._s); },
                get timeZoneId() { return __Temporal_ZonedDateTime_timeZoneId(this._s); },
                get offset() { return __Temporal_ZonedDateTime_offset(this._s); },
                get epochSeconds() { return __Temporal_ZonedDateTime_epochSeconds(this._s); },
                get epochMilliseconds() { return __Temporal_ZonedDateTime_epochMilliseconds(this._s); },
                get epochNanoseconds() { return __Temporal_ZonedDateTime_epochNanoseconds(this._s); },
                add: function (d) { return Temporal.ZonedDateTime._create(__Temporal_ZonedDateTime_add(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                subtract: function (d) { return Temporal.ZonedDateTime._create(__Temporal_ZonedDateTime_subtract(this._s, typeof d === 'object' ? d.days || 0 : d)); },
                with: function (fields) { return Temporal.ZonedDateTime._create(__Temporal_ZonedDateTime_with(this._s, fields?.year, fields?.month, fields?.day)); },
                withTimeZone: function (tz) { return Temporal.ZonedDateTime._create(__Temporal_ZonedDateTime_withTimeZone(this._s, tz)); },
                equals: function (other) { return __Temporal_ZonedDateTime_equals(this._s, other._s); },
                toString: function () { return __Temporal_ZonedDateTime_toString(this._s); },
                toJSON: function () { return __Temporal_ZonedDateTime_toJSON(this._s); },
                toInstant: function () { return Temporal.Instant._create(__Temporal_ZonedDateTime_toInstant(this._s)); },
                toPlainDateTime: function () { return Temporal.PlainDateTime.from(__Temporal_ZonedDateTime_toPlainDateTime(this._s)); },
                toPlainDate: function () { return Temporal.PlainDate.from(__Temporal_ZonedDateTime_toPlainDate(this._s)); },
                toPlainTime: function () { return Temporal.PlainTime.from(__Temporal_ZonedDateTime_toPlainTime(this._s)); },
            };
        },
    },

    Duration: {
        from: function (thing) {
            const s = __Temporal_Duration_from(String(thing));
            return Temporal.Duration._create(s);
        },
        compare: function (one, two) {
            return __Temporal_Duration_compare(one._s, two._s);
        },
        _create: function (s) {
            return {
                _s: s,
                get years() { return __Temporal_Duration_years(this._s); },
                get months() { return __Temporal_Duration_months(this._s); },
                get weeks() { return __Temporal_Duration_weeks(this._s); },
                get days() { return __Temporal_Duration_days(this._s); },
                get hours() { return __Temporal_Duration_hours(this._s); },
                get minutes() { return __Temporal_Duration_minutes(this._s); },
                get seconds() { return __Temporal_Duration_seconds(this._s); },
                get milliseconds() { return __Temporal_Duration_milliseconds(this._s); },
                get microseconds() { return __Temporal_Duration_microseconds(this._s); },
                get nanoseconds() { return __Temporal_Duration_nanoseconds(this._s); },
                get sign() { return __Temporal_Duration_sign(this._s); },
                get blank() { return __Temporal_Duration_blank(this._s); },
                negated: function () { return Temporal.Duration._create(__Temporal_Duration_negated(this._s)); },
                abs: function () { return Temporal.Duration._create(__Temporal_Duration_abs(this._s)); },
                add: function (other) { return Temporal.Duration._create(__Temporal_Duration_add(this._s, other._s || String(other))); },
                subtract: function (other) { return Temporal.Duration._create(__Temporal_Duration_subtract(this._s, other._s || String(other))); },
                round: function (opts) { return Temporal.Duration._create(__Temporal_Duration_round(this._s, opts)); },
                total: function (unit) { return __Temporal_Duration_total(this._s, unit); },
                toString: function () { return __Temporal_Duration_toString(this._s); },
                toJSON: function () { return __Temporal_Duration_toJSON(this._s); },
            };
        },
    },
};


// Symbol built-in
// Symbol constructor
globalThis.Symbol = (description) => {
    // Symbol cannot be called with new
    // In JS: if (new.target !== undefined) throw TypeError
    // For now we allow it since we don't have new.target detection
    return __Symbol_create(description);
};

// Well-known symbols as static properties
globalThis.Symbol.iterator = _symbolIterator;
globalThis.Symbol.asyncIterator = _symbolAsyncIterator;
globalThis.Symbol.toStringTag = _symbolToStringTag;
globalThis.Symbol.hasInstance = _symbolHasInstance;
globalThis.Symbol.toPrimitive = _symbolToPrimitive;
globalThis.Symbol.isConcatSpreadable = _symbolIsConcatSpreadable;
globalThis.Symbol.match = _symbolMatch;
globalThis.Symbol.matchAll = _symbolMatchAll;
globalThis.Symbol.replace = _symbolReplace;
globalThis.Symbol.search = _symbolSearch;
globalThis.Symbol.split = _symbolSplit;
globalThis.Symbol.species = _symbolSpecies;
globalThis.Symbol.unscopables = _symbolUnscopables;

// Symbol.for(key) - get or create global symbol
globalThis.Symbol.for = (key) => {
    if (typeof key !== 'string') {
        key = String(key);
    }
    return __Symbol_for(key);
};

// Symbol.keyFor(symbol) - get key for global symbol
globalThis.Symbol.keyFor = (symbol) => {
    if (typeof symbol !== 'symbol') {
        throw new TypeError('Symbol.keyFor requires a symbol');
    }
    return __Symbol_keyFor(symbol);
};

// Symbol.prototype
globalThis.Symbol.prototype = {
    // Symbol.prototype.toString()
    toString: function () {
        return __Symbol_toString(this);
    },

    // Symbol.prototype.valueOf()
    valueOf: function () {
        return __Symbol_valueOf(this);
    },

    // Symbol.prototype.description getter
    get description() {
        return __Symbol_description(this);
    },

    // Symbol.prototype[Symbol.toStringTag]
    get [_symbolToStringTag]() {
        return 'Symbol';
    },

    // Symbol.prototype[Symbol.toPrimitive]
    [_symbolToPrimitive]: function (_hint) {
        return __Symbol_valueOf(this);
    },
};

// Error built-in
function _createErrorClass(name) {
    const ErrorClass = function (message) {
        // Capture stack trace (simplified - would need VM support for full trace)
        const stack = _captureStackTrace();
        const err = __Error_create(name, message, stack);
        return err;
    };

    ErrorClass.prototype = {
        get name() {
            return __Error_getName(this);
        },
        get message() {
            return __Error_getMessage(this);
        },
        get stack() {
            return __Error_getStack(this);
        },
        toString: function () {
            return __Error_toString(this);
        },
    };

    return ErrorClass;
}

// Stack trace capture helper (simplified implementation)
function _captureStackTrace() {
    // In a full implementation, this would walk the call stack
    // For now, return a placeholder that can be enhanced later
    return undefined;
}

globalThis.Error = _createErrorClass('Error');
globalThis.TypeError = _createErrorClass('TypeError');
globalThis.ReferenceError = _createErrorClass('ReferenceError');
globalThis.SyntaxError = _createErrorClass('SyntaxError');
globalThis.RangeError = _createErrorClass('RangeError');
globalThis.URIError = _createErrorClass('URIError');
globalThis.EvalError = _createErrorClass('EvalError');

// Error.captureStackTrace (V8 compatibility)
Error.captureStackTrace = function (targetObject, constructorOpt) {
    const stack = _captureStackTrace();
    __Error_setStack(targetObject, stack || '');
};

// Function built-in
// Define Function constructor first
globalThis.Function = function Function(...args) {
    // The Function constructor creates a new function from strings
    // This is a security-sensitive operation; for now we throw
    throw new Error('Function constructor is not supported');
};

// Now set up Function.prototype
globalThis.Function.prototype = {
    // Function.prototype.call(thisArg, ...args)
    call: function (thisArg, ...args) {
        return __Function_call(this, thisArg, args);
    },

    // Function.prototype.apply(thisArg, argsArray)
    apply: function (thisArg, argsArray) {
        return __Function_apply(this, thisArg, argsArray);
    },

    // Function.prototype.bind(thisArg, a0, a1, a2, a3, a4, a5, a6, a7)
    // Creates a bound function using native __Function_createBound
    // Supports up to 8 bound arguments (common use cases)
    bind: function (thisArg, a0, a1, a2, a3, a4, a5, a6, a7) {
        const fn = this;

        if (typeof fn !== 'function') {
            throw new TypeError('Bind must be called on a function');
        }

        // Collect non-undefined bound args
        const boundArgs = [];
        if (a0 !== undefined) boundArgs[boundArgs.length] = a0;
        if (a1 !== undefined) boundArgs[boundArgs.length] = a1;
        if (a2 !== undefined) boundArgs[boundArgs.length] = a2;
        if (a3 !== undefined) boundArgs[boundArgs.length] = a3;
        if (a4 !== undefined) boundArgs[boundArgs.length] = a4;
        if (a5 !== undefined) boundArgs[boundArgs.length] = a5;
        if (a6 !== undefined) boundArgs[boundArgs.length] = a6;
        if (a7 !== undefined) boundArgs[boundArgs.length] = a7;

        // Use native bound function creation
        if (boundArgs.length === 0) {
            return __Function_createBound(fn, thisArg);
        } else if (boundArgs.length === 1) {
            return __Function_createBound(fn, thisArg, boundArgs[0]);
        } else if (boundArgs.length === 2) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1]);
        } else if (boundArgs.length === 3) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2]);
        } else if (boundArgs.length === 4) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2], boundArgs[3]);
        } else if (boundArgs.length === 5) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2], boundArgs[3], boundArgs[4]);
        } else if (boundArgs.length === 6) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2], boundArgs[3], boundArgs[4], boundArgs[5]);
        } else if (boundArgs.length === 7) {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2], boundArgs[3], boundArgs[4], boundArgs[5], boundArgs[6]);
        } else {
            return __Function_createBound(fn, thisArg, boundArgs[0], boundArgs[1], boundArgs[2], boundArgs[3], boundArgs[4], boundArgs[5], boundArgs[6], boundArgs[7]);
        }
    },

    toString: function () {
        return __Function_toString(this);
    },

    // Function.prototype.name getter
    get name() {
        return __Function_getName(this);
    },

    // Function.prototype.length getter
    get length() {
        return __Function_getLength(this);
    },
};


// Console built-in
globalThis.console = {
    log: function (...args) { return __console_log(...args); },
    error: function (...args) { return __console_error(...args); },
    warn: function (...args) { return __console_warn(...args); },
    info: function (...args) { return __console_info(...args); },
    debug: function (...args) { return __console_debug(...args); },
    trace: function (...args) { return __console_trace(...args); },
    time: function (label) { return __console_time(label); },
    timeEnd: function (label) { return __console_timeEnd(label); },
    timeLog: function (label, ...args) { return __console_timeLog(label, ...args); },
    assert: function (condition, ...args) { return __console_assert(condition, ...args); },
    clear: function () { return __console_clear(); },
    count: function (label) { return __console_count(label); },
    countReset: function (label) { return __console_countReset(label); },
    table: function (data, columns) { return __console_table(data, columns); },
    dir: function (obj, options) { return __console_dir(obj, options); },
    dirxml: function (...args) { return __console_dirxml(...args); },
    // Group methods (simplified - just log for now)
    group: function (...args) { return __console_log(...args); },
    groupCollapsed: function (...args) { return __console_log(...args); },
    groupEnd: function () { },
};

// Map built-in (ES2026)
globalThis.Map = function Map(iterable) {
    const map = __Map_new();

    // Initialize from iterable if provided
    if (iterable !== undefined && iterable !== null) {
        if (typeof iterable[_symbolIterator] === 'function') {
            for (const entry of iterable) {
                if (entry && typeof entry === 'object' && entry.length >= 2) {
                    __Map_set(map, entry[0], entry[1]);
                }
            }
        }
    }

    return {
        _internal: map,

        get size() {
            return __Map_size(this._internal);
        },

        get: function (key) {
            return __Map_get(this._internal, key);
        },

        set: function (key, value) {
            __Map_set(this._internal, key, value);
            return this;
        },

        has: function (key) {
            return __Map_has(this._internal, key);
        },

        delete: function (key) {
            return __Map_delete(this._internal, key);
        },

        clear: function () {
            __Map_clear(this._internal);
        },

        keys: function () {
            const keysArr = __Map_keys(this._internal);
            return {
                _arr: keysArr,
                _index: 0,
                next: () => {
                    // Avoid closing over locals (no upvalue capture in the VM yet).
                    const i = this._index;
                    const arr = this._arr;
                    if (i < arr.length) {
                        this._index = i + 1;
                        return { value: arr[i], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },

        values: function () {
            const valuesArr = __Map_values(this._internal);
            return {
                _arr: valuesArr,
                _index: 0,
                next: () => {
                    // Avoid closing over locals (no upvalue capture in the VM yet).
                    const i = this._index;
                    const arr = this._arr;
                    if (i < arr.length) {
                        this._index = i + 1;
                        return { value: arr[i], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },

        entries: function () {
            const entriesArr = __Map_entries(this._internal);
            return {
                _arr: entriesArr,
                _index: 0,
                next: () => {
                    // Avoid closing over locals (no upvalue capture in the VM yet).
                    const i = this._index;
                    const arr = this._arr;
                    if (i < arr.length) {
                        this._index = i + 1;
                        return { value: arr[i], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },

        forEach: function (callback, thisArg) {
            const entries = __Map_entries(this._internal);
            for (let i = 0; i < entries.length; i++) {
                const entry = entries[i];
                callback.call(thisArg, entry[1], entry[0], this);
            }
        },

        [_symbolIterator]: function () {
            return this.entries();
        },

        get [_symbolToStringTag]() {
            return 'Map';
        },
    };
};

// Map.groupBy (ES2024)
Map.groupBy = function (iterable, keySelector) {
    const map = new Map();
    let index = 0;
    for (const item of iterable) {
        const key = keySelector(item, index++);
        if (!map.has(key)) {
            map.set(key, []);
        }
        map.get(key).push(item);
    }
    return map;
};


// WeakMap built-in
globalThis.WeakMap = function WeakMap(iterable) {
    const map = __WeakMap_new();

    // Initialize from iterable if provided
    if (iterable !== undefined && iterable !== null) {
        if (typeof iterable[_symbolIterator] === 'function') {
            for (const entry of iterable) {
                if (entry && typeof entry === 'object' && entry.length >= 2) {
                    __WeakMap_set(map, entry[0], entry[1]);
                }
            }
        }
    }

    return {
        _internal: map,

        get: function (key) {
            return __WeakMap_get(this._internal, key);
        },

        set: function (key, value) {
            __WeakMap_set(this._internal, key, value);
            return this;
        },

        has: function (key) {
            return __WeakMap_has(this._internal, key);
        },

        delete: function (key) {
            return __WeakMap_delete(this._internal, key);
        },

        get [_symbolToStringTag]() {
            return 'WeakMap';
        },
    };
};


// Set built-in (ES2026 with ES2025 set methods)
globalThis.Set = function Set(iterable) {
    const set = __Set_new();

    // Initialize from iterable if provided
    if (iterable !== undefined && iterable !== null) {
        if (typeof iterable[_symbolIterator] === 'function') {
            for (const value of iterable) {
                __Set_add(set, value);
            }
        }
    }

    return {
        _internal: set,

        get size() {
            return __Set_size(this._internal);
        },

        add: function (value) {
            __Set_add(this._internal, value);
            return this;
        },

        has: function (value) {
            return __Set_has(this._internal, value);
        },

        delete: function (value) {
            return __Set_delete(this._internal, value);
        },

        clear: function () {
            __Set_clear(this._internal);
        },

        values: function () {
            const valuesArr = __Set_values(this._internal);
            let index = 0;
            return {
                next: () => {
                    if (index < valuesArr.length) {
                        return { value: valuesArr[index++], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },

        keys: function () {
            // keys() is an alias for values() in Set
            return this.values();
        },

        entries: function () {
            const entriesArr = __Set_entries(this._internal);
            let index = 0;
            return {
                next: () => {
                    if (index < entriesArr.length) {
                        return { value: entriesArr[index++], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },

        forEach: function (callback, thisArg) {
            const values = __Set_values(this._internal);
            for (let i = 0; i < values.length; i++) {
                const value = values[i];
                callback.call(thisArg, value, value, this);
            }
        },

        // ES2025 Set methods
        union: function (other) {
            return Set._fromInternal(__Set_union(this._internal, other._internal));
        },

        intersection: function (other) {
            return Set._fromInternal(__Set_intersection(this._internal, other._internal));
        },

        difference: function (other) {
            return Set._fromInternal(__Set_difference(this._internal, other._internal));
        },

        symmetricDifference: function (other) {
            return Set._fromInternal(__Set_symmetricDifference(this._internal, other._internal));
        },

        isSubsetOf: function (other) {
            return __Set_isSubsetOf(this._internal, other._internal);
        },

        isSupersetOf: function (other) {
            return __Set_isSupersetOf(this._internal, other._internal);
        },

        isDisjointFrom: function (other) {
            return __Set_isDisjointFrom(this._internal, other._internal);
        },

        [_symbolIterator]: function () {
            return this.values();
        },

        get [_symbolToStringTag]() {
            return 'Set';
        },
    };
};

// Helper to create Set from internal representation
Set._fromInternal = function (internal) {
    const set = {
        _internal: internal,
        get size() { return __Set_size(this._internal); },
        add: function (value) { __Set_add(this._internal, value); return this; },
        has: function (value) { return __Set_has(this._internal, value); },
        delete: function (value) { return __Set_delete(this._internal, value); },
        clear: function () { __Set_clear(this._internal); },
        values: function () {
            const valuesArr = __Set_values(this._internal);
            let index = 0;
            return {
                next: () => {
                    if (index < valuesArr.length) {
                        return { value: valuesArr[index++], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },
        keys: function () { return this.values(); },
        entries: function () {
            const entriesArr = __Set_entries(this._internal);
            let index = 0;
            return {
                next: () => {
                    if (index < entriesArr.length) {
                        return { value: entriesArr[index++], done: false };
                    }
                    return { value: undefined, done: true };
                },
                [_symbolIterator]: function () { return this; }
            };
        },
        forEach: function (callback, thisArg) {
            const values = __Set_values(this._internal);
            for (let i = 0; i < values.length; i++) {
                callback.call(thisArg, values[i], values[i], this);
            }
        },
        union: function (other) { return Set._fromInternal(__Set_union(this._internal, other._internal)); },
        intersection: function (other) { return Set._fromInternal(__Set_intersection(this._internal, other._internal)); },
        difference: function (other) { return Set._fromInternal(__Set_difference(this._internal, other._internal)); },
        symmetricDifference: function (other) { return Set._fromInternal(__Set_symmetricDifference(this._internal, other._internal)); },
        isSubsetOf: function (other) { return __Set_isSubsetOf(this._internal, other._internal); },
        isSupersetOf: function (other) { return __Set_isSupersetOf(this._internal, other._internal); },
        isDisjointFrom: function (other) { return __Set_isDisjointFrom(this._internal, other._internal); },
        [_symbolIterator]: function () { return this.values(); },
        get [_symbolToStringTag]() { return 'Set'; },
    };
    return set;
};


// WeakSet built-in
globalThis.WeakSet = function WeakSet(iterable) {
    const set = __WeakSet_new();

    // Initialize from iterable if provided
    if (iterable !== undefined && iterable !== null) {
        if (typeof iterable[_symbolIterator] === 'function') {
            for (const value of iterable) {
                __WeakSet_add(set, value);
            }
        }
    }

    return {
        _internal: set,

        add: function (value) {
            __WeakSet_add(this._internal, value);
            return this;
        },

        has: function (value) {
            return __WeakSet_has(this._internal, value);
        },

        delete: function (value) {
            return __WeakSet_delete(this._internal, value);
        },

        get [_symbolToStringTag]() {
            return 'WeakSet';
        },
    };
};


// Promise built-in
globalThis.Promise = function Promise(executor) {
    if (typeof executor !== 'function') {
        throw new TypeError('Promise resolver is not a function');
    }

    // Create the internal promise
    const _promise = __Promise_create();

    const obj = {
        _internal: _promise,

        then: function (onFulfilled, onRejected) {
            const state = __Promise_state(this._internal);

            // Create a new promise for chaining
            const chainPromise = __Promise_create();

            // Helper to handle callback result
            const handleCallback = (callback, value, isReject) => {
                if (typeof callback !== 'function') {
                    // Pass through
                    if (isReject) {
                        __Promise_reject(chainPromise, value);
                    } else {
                        __Promise_resolve(chainPromise, value);
                    }
                    return;
                }

                try {
                    const result = callback(value);
                    // Check if result is a promise (thenable)
                    if (result && typeof result.then === 'function') {
                        result.then(
                            (v) => __Promise_resolve(chainPromise, v),
                            (e) => __Promise_reject(chainPromise, e)
                        );
                    } else {
                        __Promise_resolve(chainPromise, result);
                    }
                } catch (e) {
                    __Promise_reject(chainPromise, e);
                }
            };

            if (state.state === 'fulfilled') {
                // Already fulfilled - schedule callback
                queueMicrotask(() => handleCallback(onFulfilled, state.value, false));
            } else if (state.state === 'rejected') {
                // Already rejected - schedule callback
                queueMicrotask(() => handleCallback(onRejected, state.reason, true));
            } else {
                // Pending - register callbacks
                // Store callbacks in the promise for later execution
                const _chainPromise = chainPromise;
                const _onFulfilled = onFulfilled;
                const _onRejected = onRejected;

                // We need to monitor state changes
                // This is a simplified implementation
                const checkState = () => {
                    const s = __Promise_state(this._internal);
                    if (s.state === 'fulfilled') {
                        handleCallback(_onFulfilled, s.value, false);
                    } else if (s.state === 'rejected') {
                        handleCallback(_onRejected, s.reason, true);
                    } else {
                        // Still pending - check again later
                        queueMicrotask(checkState);
                    }
                };
                queueMicrotask(checkState);
            }

            // Return a Promise wrapper around chainPromise
            return Promise._fromInternal(chainPromise);
        },

        catch: function (onRejected) {
            return this.then(undefined, onRejected);
        },

        finally: function (onFinally) {
            return this.then(
                (value) => {
                    if (typeof onFinally === 'function') {
                        const result = onFinally();
                        if (result && typeof result.then === 'function') {
                            return result.then(() => value);
                        }
                    }
                    return value;
                },
                (reason) => {
                    if (typeof onFinally === 'function') {
                        const result = onFinally();
                        if (result && typeof result.then === 'function') {
                            return result.then(() => { throw reason; });
                        }
                    }
                    throw reason;
                }
            );
        },

        get [_symbolToStringTag]() {
            return 'Promise';
        },
    };

    // Execute the executor
    try {
        executor(
            (value) => __Promise_resolve(_promise, value),
            (reason) => __Promise_reject(_promise, reason)
        );
    } catch (e) {
        __Promise_reject(_promise, e);
    }

    return obj;
};

// Helper to wrap an internal promise
Promise._fromInternal = function (internal) {
    return {
        _internal: internal,
        then: globalThis.Promise.prototype.then,
        catch: globalThis.Promise.prototype.catch,
        finally: globalThis.Promise.prototype.finally,
        get [_symbolToStringTag]() { return 'Promise'; },
    };
};

// Promise.prototype for _fromInternal
Promise.prototype = {
    then: function (onFulfilled, onRejected) {
        const state = __Promise_state(this._internal);
        const chainPromise = __Promise_create();

        const handleCallback = (callback, value, isReject) => {
            if (typeof callback !== 'function') {
                if (isReject) {
                    __Promise_reject(chainPromise, value);
                } else {
                    __Promise_resolve(chainPromise, value);
                }
                return;
            }

            try {
                const result = callback(value);
                if (result && typeof result.then === 'function') {
                    result.then(
                        (v) => __Promise_resolve(chainPromise, v),
                        (e) => __Promise_reject(chainPromise, e)
                    );
                } else {
                    __Promise_resolve(chainPromise, result);
                }
            } catch (e) {
                __Promise_reject(chainPromise, e);
            }
        };

        if (state.state === 'fulfilled') {
            queueMicrotask(() => handleCallback(onFulfilled, state.value, false));
        } else if (state.state === 'rejected') {
            queueMicrotask(() => handleCallback(onRejected, state.reason, true));
        } else {
            const self = this;
            const checkState = () => {
                const s = __Promise_state(self._internal);
                if (s.state === 'fulfilled') {
                    handleCallback(onFulfilled, s.value, false);
                } else if (s.state === 'rejected') {
                    handleCallback(onRejected, s.reason, true);
                } else {
                    queueMicrotask(checkState);
                }
            };
            queueMicrotask(checkState);
        }

        return Promise._fromInternal(chainPromise);
    },

    catch: function (onRejected) {
        return this.then(undefined, onRejected);
    },

    finally: function (onFinally) {
        return this.then(
            (value) => {
                if (typeof onFinally === 'function') {
                    const result = onFinally();
                    if (result && typeof result.then === 'function') {
                        return result.then(() => value);
                    }
                }
                return value;
            },
            (reason) => {
                if (typeof onFinally === 'function') {
                    const result = onFinally();
                    if (result && typeof result.then === 'function') {
                        return result.then(() => { throw reason; });
                    }
                }
                throw reason;
            }
        );
    },
};

// Promise.resolve
Promise.resolve = function (value) {
    // If value is already a Promise, return it
    if (value && value._internal && typeof value.then === 'function') {
        return value;
    }
    // Create a resolved promise
    const result = __Promise_resolve(value);
    return Promise._fromInternal(result._internal || result);
};

// Promise.reject
Promise.reject = function (reason) {
    const result = __Promise_reject(reason);
    return Promise._fromInternal(result._internal || result);
};

// Promise.all
Promise.all = function (iterable) {
    const arr = Array.isArray(iterable) ? iterable : Array.from(iterable);
    const result = __Promise_all(arr.map(p => p && p._internal ? p._internal : p));
    return Promise._fromInternal(result._internal || result);
};

// Promise.race
Promise.race = function (iterable) {
    const arr = Array.isArray(iterable) ? iterable : Array.from(iterable);
    const result = __Promise_race(arr.map(p => p && p._internal ? p._internal : p));
    return Promise._fromInternal(result._internal || result);
};

// Promise.allSettled
Promise.allSettled = function (iterable) {
    const arr = Array.isArray(iterable) ? iterable : Array.from(iterable);
    const result = __Promise_allSettled(arr.map(p => p && p._internal ? p._internal : p));
    return Promise._fromInternal(result._internal || result);
};

// Promise.any
Promise.any = function (iterable) {
    const arr = Array.isArray(iterable) ? iterable : Array.from(iterable);
    const result = __Promise_any(arr.map(p => p && p._internal ? p._internal : p));
    return Promise._fromInternal(result._internal || result);
};

// Promise.withResolvers (ES2024)
Promise.withResolvers = function () {
    const resolvers = __Promise_withResolvers();
    return {
        promise: Promise._fromInternal(resolvers.promise._internal || resolvers.promise),
        resolve: resolvers.resolve,
        reject: resolvers.reject,
    };
};

// queueMicrotask global (used by Promise)
// Synchronous execution fallback - a proper implementation should use
// the event loop's microtask queue via a native function.
if (typeof globalThis.queueMicrotask === 'undefined') {
    globalThis.queueMicrotask = function (callback) {
        // Simply execute synchronously (not spec-compliant but avoids recursion)
        callback();
    };
}


// ============================================================================
// Proxy built-in
// ============================================================================

globalThis.Proxy = function (target, handler) {
    if (new.target === undefined) {
        throw new TypeError("Constructor Proxy requires 'new'");
    }
    if (target === null || (typeof target !== 'object' && typeof target !== 'function')) {
        throw new TypeError('Proxy target must be an object');
    }
    if (handler === null || typeof handler !== 'object') {
        throw new TypeError('Proxy handler must be an object');
    }
    return __Proxy_create(target, handler);
};

// Proxy.revocable
Proxy.revocable = function (target, handler) {
    if (target === null || (typeof target !== 'object' && typeof target !== 'function')) {
        throw new TypeError('Proxy target must be an object');
    }
    if (handler === null || typeof handler !== 'object') {
        throw new TypeError('Proxy handler must be an object');
    }
    return __Proxy_revocable(target, handler);
};


// ============================================================================

// ============================================================================
// Generator / Iterator Protocol Support
// ============================================================================

// Generator prototype methods (attached to generator instances by the runtime)
globalThis.GeneratorPrototype = {
    next: function (value) {
        return __Generator_next(this, value);
    },

    return: function (value) {
        return __Generator_return(this, value);
    },

    throw: function (exception) {
        return __Generator_throw(this, exception);
    },

    // Symbol.iterator returns the generator itself
    [_symbolIterator]() {
        return this;
    },

    get [_symbolToStringTag]() {
        return 'Generator';
    },
};
if (GeneratorPrototype) {
}

try {
    __markNonConstructor(GeneratorPrototype.next);
    __markNonConstructor(GeneratorPrototype.return);
    __markNonConstructor(GeneratorPrototype.throw);
    __markNonConstructor(GeneratorPrototype[_symbolIterator]);
} catch (e) {
    // Don't rethrow for now, to see if we can continue
}

// GeneratorFunction.prototype - the prototype of all generator functions
// When you do Object.getPrototypeOf(function* g() {}), you get this object
globalThis.GeneratorFunctionPrototype = {
    // The prototype property of generator functions should be GeneratorPrototype
    prototype: globalThis.GeneratorPrototype,

    // Generator functions aren't constructable
    constructor: undefined,

    get [_symbolToStringTag]() {
        return 'GeneratorFunction';
    },
};

// AsyncGeneratorFunction.prototype - the prototype of all async generator functions
globalThis.AsyncGeneratorFunctionPrototype = {
    prototype: globalThis.GeneratorPrototype,

    constructor: undefined,

    get [_symbolToStringTag]() {
        return 'AsyncGeneratorFunction';
    },
};

// Internal: IteratorPrototype for custom iterators
globalThis.IteratorPrototype = {
    [_symbolIterator]() {
        return this;
    },
};

// Helper function to create iterator results
globalThis.__createIteratorResult = (value, done) => {
    if (done) {
        return __Iterator_done(value);
    }
    return __Iterator_result(value);
};

// Check if a value is a generator
globalThis.__isGenerator = (value) => __Generator_isGenerator(value);


// ArrayBuffer built-in
globalThis.ArrayBuffer = function ArrayBuffer(length, options) {
    // ArrayBuffer must be called with 'new'
    if (!new.target) {
        throw new TypeError("Constructor ArrayBuffer requires 'new'");
    }

    const byteLength = length === undefined ? 0 : Number(length);
    if (!Number.isFinite(byteLength) || byteLength < 0) {
        throw new RangeError('Invalid array buffer length');
    }

    let maxByteLength;
    if (options !== undefined && options !== null && typeof options === 'object') {
        maxByteLength = options.maxByteLength;
        if (maxByteLength !== undefined) {
            maxByteLength = Number(maxByteLength);
            if (!Number.isFinite(maxByteLength) || maxByteLength < 0) {
                throw new RangeError('Invalid maxByteLength');
            }
        }
    }

    const _internal = __ArrayBuffer_create(byteLength, maxByteLength);

    const obj = Object.create(ArrayBuffer.prototype);
    obj._internal = _internal;

    return obj;
};

ArrayBuffer.prototype = {
    get byteLength() {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.byteLength requires that "this" be an ArrayBuffer');
        }
        return __ArrayBuffer_byteLength(this._internal);
    },

    get maxByteLength() {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.maxByteLength requires that "this" be an ArrayBuffer');
        }
        return __ArrayBuffer_maxByteLength(this._internal);
    },

    get resizable() {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.resizable requires that "this" be an ArrayBuffer');
        }
        return __ArrayBuffer_resizable(this._internal);
    },

    get detached() {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.detached requires that "this" be an ArrayBuffer');
        }
        return __ArrayBuffer_detached(this._internal);
    },

    slice(begin, end) {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.slice requires that "this" be an ArrayBuffer');
        }
        const newInternal = __ArrayBuffer_slice(this._internal, begin, end);
        const result = Object.create(ArrayBuffer.prototype);
        result._internal = newInternal;
        return result;
    },

    transfer(newLength) {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.transfer requires that "this" be an ArrayBuffer');
        }
        const newInternal = __ArrayBuffer_transfer(this._internal);
        const result = Object.create(ArrayBuffer.prototype);
        result._internal = newInternal;
        return result;
    },

    transferToFixedLength(newLength) {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.transferToFixedLength requires that "this" be an ArrayBuffer');
        }
        const newInternal = __ArrayBuffer_transferToFixedLength(this._internal, newLength);
        const result = Object.create(ArrayBuffer.prototype);
        result._internal = newInternal;
        return result;
    },

    resize(newLength) {
        if (!this._internal) {
            throw new TypeError('ArrayBuffer.prototype.resize requires that "this" be an ArrayBuffer');
        }
        __ArrayBuffer_resize(this._internal, newLength);
    },

    get [_symbolToStringTag]() {
        return 'ArrayBuffer';
    },
};

if (Object) {
}

try {
    Object.defineProperty(ArrayBuffer.prototype, 'constructor', {
        value: ArrayBuffer,
        writable: true,
        enumerable: false,
        configurable: true,
    });
} catch (e) {
}

try {
    ArrayBuffer.isView = __markNonConstructor(function isView(arg) {
        return __ArrayBuffer_isView(arg);
    });
} catch (e) {
}

// Helper to create ArrayBuffer from internal value (for slice/transfer operations)
ArrayBuffer._fromInternal = function (internal) {
    const result = Object.create(ArrayBuffer.prototype);
    result._internal = internal;
    return result;
};

// ============================================================================
// TypedArray built-ins
// ============================================================================

// %TypedArray% intrinsic - base for all typed array types
const TypedArrayPrototype = {
    get buffer() {
        return __TypedArray_buffer(this._internal);
    },

    get byteLength() {
        return __TypedArray_byteLength(this._internal);
    },

    get byteOffset() {
        return __TypedArray_byteOffset(this._internal);
    },

    get length() {
        return __TypedArray_length(this._internal);
    },

    at(index) {
        const len = this.length;
        const k = index >= 0 ? index : len + index;
        if (k < 0 || k >= len) return undefined;
        return __TypedArray_get(this._internal, k);
    },

    subarray(begin, end) {
        const result = __TypedArray_subarray(this._internal, begin, end);
        return this.constructor._fromInternal(result);
    },

    slice(begin, end) {
        const result = __TypedArray_slice(this._internal, begin, end);
        return this.constructor._fromInternal(result);
    },

    fill(value, start, end) {
        __TypedArray_fill(this._internal, value, start, end);
        return this;
    },

    copyWithin(target, start, end) {
        __TypedArray_copyWithin(this._internal, target, start, end);
        return this;
    },

    reverse() {
        __TypedArray_reverse(this._internal);
        return this;
    },

    set(source, offset) {
        if (offset === undefined) offset = 0;
        if (source._internal && __TypedArray_isTypedArray(source._internal)) {
            // TypedArray source
            const srcLen = source.length;
            for (let i = 0; i < srcLen; i++) {
                __TypedArray_set(this._internal, offset + i, __TypedArray_get(source._internal, i));
            }
        } else {
            // Array-like source
            __TypedArray_set_array(this._internal, source, offset);
        }
    },

    indexOf(searchElement, fromIndex) {
        const len = this.length;
        let k = fromIndex === undefined ? 0 : Math.trunc(fromIndex);
        if (k < 0) k = Math.max(len + k, 0);
        for (; k < len; k++) {
            const elem = __TypedArray_get(this._internal, k);
            if (elem === searchElement) return k;
        }
        return -1;
    },

    lastIndexOf(searchElement, fromIndex) {
        const len = this.length;
        let k = fromIndex === undefined ? len - 1 : Math.trunc(fromIndex);
        if (k < 0) k = len + k;
        for (; k >= 0; k--) {
            const elem = __TypedArray_get(this._internal, k);
            if (elem === searchElement) return k;
        }
        return -1;
    },

    includes(searchElement, fromIndex) {
        return this.indexOf(searchElement, fromIndex) !== -1;
    },

    join(separator) {
        const sep = separator === undefined ? ',' : String(separator);
        const len = this.length;
        if (len === 0) return '';
        let result = String(__TypedArray_get(this._internal, 0));
        for (let i = 1; i < len; i++) {
            result += sep + String(__TypedArray_get(this._internal, i));
        }
        return result;
    },

    toString() {
        return this.join(',');
    },

    forEach(callback, thisArg) {
        const len = this.length;
        for (let i = 0; i < len; i++) {
            callback.call(thisArg, __TypedArray_get(this._internal, i), i, this);
        }
    },

    map(callback, thisArg) {
        const len = this.length;
        const result = new this.constructor(len);
        for (let i = 0; i < len; i++) {
            const mapped = callback.call(thisArg, __TypedArray_get(this._internal, i), i, this);
            __TypedArray_set(result._internal, i, mapped);
        }
        return result;
    },

    filter(callback, thisArg) {
        const len = this.length;
        const kept = [];
        for (let i = 0; i < len; i++) {
            const val = __TypedArray_get(this._internal, i);
            if (callback.call(thisArg, val, i, this)) {
                kept.push(val);
            }
        }
        const result = new this.constructor(kept.length);
        for (let i = 0; i < kept.length; i++) {
            __TypedArray_set(result._internal, i, kept[i]);
        }
        return result;
    },

    reduce(callback, initialValue) {
        const len = this.length;
        let k = 0;
        let accumulator;
        if (arguments.length >= 2) {
            accumulator = initialValue;
        } else {
            if (len === 0) throw new TypeError('Reduce of empty array with no initial value');
            accumulator = __TypedArray_get(this._internal, 0);
            k = 1;
        }
        for (; k < len; k++) {
            accumulator = callback(accumulator, __TypedArray_get(this._internal, k), k, this);
        }
        return accumulator;
    },

    reduceRight(callback, initialValue) {
        const len = this.length;
        let k = len - 1;
        let accumulator;
        if (arguments.length >= 2) {
            accumulator = initialValue;
        } else {
            if (len === 0) throw new TypeError('Reduce of empty array with no initial value');
            accumulator = __TypedArray_get(this._internal, len - 1);
            k = len - 2;
        }
        for (; k >= 0; k--) {
            accumulator = callback(accumulator, __TypedArray_get(this._internal, k), k, this);
        }
        return accumulator;
    },

    find(callback, thisArg) {
        const len = this.length;
        for (let i = 0; i < len; i++) {
            const val = __TypedArray_get(this._internal, i);
            if (callback.call(thisArg, val, i, this)) return val;
        }
        return undefined;
    },

    findIndex(callback, thisArg) {
        const len = this.length;
        for (let i = 0; i < len; i++) {
            if (callback.call(thisArg, __TypedArray_get(this._internal, i), i, this)) return i;
        }
        return -1;
    },

    findLast(callback, thisArg) {
        const len = this.length;
        for (let i = len - 1; i >= 0; i--) {
            const val = __TypedArray_get(this._internal, i);
            if (callback.call(thisArg, val, i, this)) return val;
        }
        return undefined;
    },

    findLastIndex(callback, thisArg) {
        const len = this.length;
        for (let i = len - 1; i >= 0; i--) {
            if (callback.call(thisArg, __TypedArray_get(this._internal, i), i, this)) return i;
        }
        return -1;
    },

    every(callback, thisArg) {
        const len = this.length;
        for (let i = 0; i < len; i++) {
            if (!callback.call(thisArg, __TypedArray_get(this._internal, i), i, this)) return false;
        }
        return true;
    },

    some(callback, thisArg) {
        const len = this.length;
        for (let i = 0; i < len; i++) {
            if (callback.call(thisArg, __TypedArray_get(this._internal, i), i, this)) return true;
        }
        return false;
    },

    sort(compareFn) {
        const len = this.length;
        const arr = [];
        for (let i = 0; i < len; i++) {
            arr.push(__TypedArray_get(this._internal, i));
        }
        if (compareFn === undefined) {
            arr.sort((a, b) => a - b);
        } else {
            arr.sort(compareFn);
        }
        for (let i = 0; i < len; i++) {
            __TypedArray_set(this._internal, i, arr[i]);
        }
        return this;
    },

    toReversed() {
        const copy = this.slice();
        copy.reverse();
        return copy;
    },

    toSorted(compareFn) {
        const copy = this.slice();
        copy.sort(compareFn);
        return copy;
    },

    with(index, value) {
        const len = this.length;
        const k = index >= 0 ? index : len + index;
        if (k < 0 || k >= len) throw new RangeError('Invalid index');
        const copy = this.slice();
        __TypedArray_set(copy._internal, k, value);
        return copy;
    },

    [_symbolIterator]() {
        let index = 0;
        const arr = this;
        return {
            next() {
                if (index < arr.length) {
                    return { value: __TypedArray_get(arr._internal, index++), done: false };
                }
                return { value: undefined, done: true };
            },
            [_symbolIterator]() { return this; }
        };
    },

    entries() {
        let index = 0;
        const arr = this;
        return {
            next() {
                if (index < arr.length) {
                    const val = __TypedArray_get(arr._internal, index);
                    return { value: [index++, val], done: false };
                }
                return { value: undefined, done: true };
            },
            [_symbolIterator]() { return this; }
        };
    },

    keys() {
        let index = 0;
        const arr = this;
        return {
            next() {
                if (index < arr.length) {
                    return { value: index++, done: false };
                }
                return { value: undefined, done: true };
            },
            [_symbolIterator]() { return this; }
        };
    },

    values() {
        return this[_symbolIterator]();
    },
};

// Factory to create TypedArray constructors
function __createTypedArrayConstructor(name, bytesPerElement) {
    const Constructor = function (arg, byteOffset, length) {
        if (!new.target) {
            throw new TypeError(`Constructor ${name} requires 'new'`);
        }

        let internal;

        if (arg === undefined || arg === null) {
            // new TypedArray() - empty array
            internal = __TypedArray_createFromLength(0, name);
        } else if (typeof arg === 'number') {
            // new TypedArray(length)
            if (!Number.isInteger(arg) || arg < 0) {
                throw new RangeError('Invalid typed array length');
            }
            internal = __TypedArray_createFromLength(arg, name);
        } else if (arg._internal && arg._internal.constructor && arg._internal.constructor.name === 'ArrayBuffer') {
            // new TypedArray(buffer, byteOffset?, length?)
            internal = __TypedArray_create(arg._internal, name, byteOffset, length);
        } else if (ArrayBuffer.prototype.isPrototypeOf(arg)) {
            // Direct ArrayBuffer (internal)
            internal = __TypedArray_create(arg._internal, name, byteOffset, length);
        } else if (__TypedArray_isTypedArray(arg._internal)) {
            // new TypedArray(typedArray) - copy
            const srcLen = arg.length;
            internal = __TypedArray_createFromLength(srcLen, name);
            for (let i = 0; i < srcLen; i++) {
                __TypedArray_set(internal, i, __TypedArray_get(arg._internal, i));
            }
        } else if (typeof arg === 'object') {
            // new TypedArray(arrayLike) or new TypedArray(iterable)
            const arr = Array.from(arg);
            internal = __TypedArray_createFromLength(arr.length, name);
            for (let i = 0; i < arr.length; i++) {
                __TypedArray_set(internal, i, arr[i]);
            }
        } else {
            throw new TypeError('Invalid argument for TypedArray constructor');
        }

        const obj = Object.create(Constructor.prototype);
        obj._internal = internal;
        return obj;
    };

    Constructor.prototype = globalThis.Object.create(TypedArrayPrototype);
    Constructor.prototype.constructor = Constructor;

    globalThis.Object.defineProperty(Constructor.prototype, _symbolToStringTag, {
        get() { return name; },
        configurable: true
    });

    Constructor.BYTES_PER_ELEMENT = bytesPerElement;
    globalThis.Object.defineProperty(Constructor.prototype, 'BYTES_PER_ELEMENT', {
        value: bytesPerElement,
        writable: false,
        enumerable: false,
        configurable: false
    });

    // Static methods
    Constructor.of = function (...items) {
        const result = new Constructor(items.length);
        for (let i = 0; i < items.length; i++) {
            __TypedArray_set(result._internal, i, items[i]);
        }
        return result;
    };

    Constructor.from = function (source, mapFn, thisArg) {
        const arr = Array.from(source);
        const len = arr.length;
        const result = new Constructor(len);
        for (let i = 0; i < len; i++) {
            const val = mapFn ? mapFn.call(thisArg, arr[i], i) : arr[i];
            __TypedArray_set(result._internal, i, val);
        }
        return result;
    };

    Constructor._fromInternal = function (internal) {
        const obj = Object.create(Constructor.prototype);
        obj._internal = internal;
        return obj;
    };

    // Explicitly return Constructor
    return Constructor;
}


// Create all 11 TypedArray constructors
globalThis.Int8Array = __createTypedArrayConstructor('Int8Array', 1);
globalThis.Uint8Array = __createTypedArrayConstructor('Uint8Array', 1);
globalThis.Uint8ClampedArray = __createTypedArrayConstructor('Uint8ClampedArray', 1);
globalThis.Int16Array = __createTypedArrayConstructor('Int16Array', 2);
globalThis.Uint16Array = __createTypedArrayConstructor('Uint16Array', 2);
globalThis.Int32Array = __createTypedArrayConstructor('Int32Array', 4);
globalThis.Uint32Array = __createTypedArrayConstructor('Uint32Array', 4);
globalThis.Float32Array = __createTypedArrayConstructor('Float32Array', 4);
globalThis.Float64Array = __createTypedArrayConstructor('Float64Array', 8);
globalThis.BigInt64Array = __createTypedArrayConstructor('BigInt64Array', 8);
globalThis.BigUint64Array = __createTypedArrayConstructor('BigUint64Array', 8);


// ===== DataView =====

globalThis.DataView = (function () {
    function DataViewConstructor(buffer, byteOffset, byteLength) {
        if (!new.target) {
            throw new TypeError("Constructor DataView requires 'new'");
        }

        if (!(buffer instanceof ArrayBuffer)) {
            throw new TypeError("First argument must be an ArrayBuffer");
        }

        const internal = __DataView_create(buffer._internal, byteOffset, byteLength);

        Object.defineProperty(this, '_internal', {
            value: internal,
            writable: false,
            enumerable: false,
            configurable: false,
        });
    }
    return DataViewConstructor;
})();

Object.defineProperty(globalThis.globalThis.DataView.prototype, 'buffer', {
    get: function () {
        if (!this._internal || !__DataView_isDataView(this._internal)) {
            throw new TypeError('get globalThis.DataView.prototype.buffer called on incompatible receiver');
        }
        const ab = __DataView_getBuffer(this._internal);
        // Wrap in ArrayBuffer-like object
        const result = Object.create(ArrayBuffer.prototype);
        Object.defineProperty(result, '_internal', { value: ab, writable: false, enumerable: false, configurable: false });
        return result;
    },
    enumerable: false,
    configurable: true,
});

Object.defineProperty(globalThis.DataView.prototype, 'byteOffset', {
    get: function () {
        if (!this._internal || !__DataView_isDataView(this._internal)) {
            throw new TypeError('get globalThis.DataView.prototype.byteOffset called on incompatible receiver');
        }
        return __DataView_getByteOffset(this._internal);
    },
    enumerable: false,
    configurable: true,
});

Object.defineProperty(globalThis.DataView.prototype, 'byteLength', {
    get: function () {
        if (!this._internal || !__DataView_isDataView(this._internal)) {
            throw new TypeError('get globalThis.DataView.prototype.byteLength called on incompatible receiver');
        }
        return __DataView_getByteLength(this._internal);
    },
    enumerable: false,
    configurable: true,
});

// Get methods
globalThis.DataView.prototype.getInt8 = function (byteOffset) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getInt8(this._internal, byteOffset);
};

globalThis.DataView.prototype.getUint8 = function (byteOffset) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getUint8(this._internal, byteOffset);
};

globalThis.DataView.prototype.getInt16 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getInt16(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getUint16 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getUint16(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getInt32 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getInt32(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getUint32 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getUint32(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getFloat32 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getFloat32(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getFloat64 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getFloat64(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getBigInt64 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getBigInt64(this._internal, byteOffset, littleEndian);
};

globalThis.DataView.prototype.getBigUint64 = function (byteOffset, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    return __DataView_getBigUint64(this._internal, byteOffset, littleEndian);
};

// Set methods
globalThis.DataView.prototype.setInt8 = function (byteOffset, value) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setInt8(this._internal, byteOffset, value);
};

globalThis.DataView.prototype.setUint8 = function (byteOffset, value) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setUint8(this._internal, byteOffset, value);
};

globalThis.DataView.prototype.setInt16 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setInt16(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setUint16 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setUint16(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setInt32 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setInt32(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setUint32 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setUint32(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setFloat32 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setFloat32(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setFloat64 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setFloat64(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setBigInt64 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setBigInt64(this._internal, byteOffset, value, littleEndian);
};

globalThis.DataView.prototype.setBigUint64 = function (byteOffset, value, littleEndian) {
    if (!this._internal) throw new TypeError('not a DataView');
    __DataView_setBigUint64(this._internal, byteOffset, value, littleEndian);
};

Object.defineProperty(globalThis.DataView.prototype, Symbol.toStringTag, {
    value: 'DataView',
    writable: false,
    enumerable: false,
    configurable: true,
});

// Update ArrayBuffer.isView to detect TypedArrays and DataView
ArrayBuffer.isView = __markNonConstructor(function isView(arg) {
    if (arg === null || arg === undefined) return false;
    if (arg._internal && __TypedArray_isTypedArray(arg._internal)) return true;
    if (arg._internal && __DataView_isDataView(arg._internal)) return true;
    return false;
});