// Object built-in wrapper
globalThis.Object = {
    keys: function(obj) {
        return __Object_keys(obj);
    },
    values: function(obj) {
        return __Object_values(obj);
    },
    entries: function(obj) {
        return __Object_entries(obj);
    },
    assign: function(target, ...sources) {
        return __Object_assign(target, ...sources);
    },
    hasOwn: function(obj, key) {
        return __Object_hasOwn(obj, key);
    },
    // Object mutability methods (native ops)
    freeze: function(obj) {
        return __Object_freeze(obj);
    },
    isFrozen: function(obj) {
        return __Object_isFrozen(obj);
    },
    seal: function(obj) {
        return __Object_seal(obj);
    },
    isSealed: function(obj) {
        return __Object_isSealed(obj);
    },
    preventExtensions: function(obj) {
        return __Object_preventExtensions(obj);
    },
    isExtensible: function(obj) {
        return __Object_isExtensible(obj);
    },
};

// Array built-in wrapper
globalThis.Array = {
    isArray: function(val) {
        return __Array_isArray(val);
    },
    from: function(arrayLike) {
        return __Array_from(arrayLike);
    },
    of: function(...items) {
        return __Array_of(items);
    },
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
    abs: function(x) { return __Math_abs(x); },
    ceil: function(x) { return __Math_ceil(x); },
    floor: function(x) { return __Math_floor(x); },
    round: function(x) { return __Math_round(x); },
    trunc: function(x) { return __Math_trunc(x); },
    sign: function(x) { return __Math_sign(x); },

    // Roots and Powers
    sqrt: function(x) { return __Math_sqrt(x); },
    cbrt: function(x) { return __Math_cbrt(x); },
    pow: function(base, exp) { return __Math_pow(base, exp); },
    hypot: function(...values) { return __Math_hypot(...values); },

    // Exponentials and Logarithms
    exp: function(x) { return __Math_exp(x); },
    expm1: function(x) { return __Math_expm1(x); },
    log: function(x) { return __Math_log(x); },
    log1p: function(x) { return __Math_log1p(x); },
    log2: function(x) { return __Math_log2(x); },
    log10: function(x) { return __Math_log10(x); },

    // Trigonometry
    sin: function(x) { return __Math_sin(x); },
    cos: function(x) { return __Math_cos(x); },
    tan: function(x) { return __Math_tan(x); },
    asin: function(x) { return __Math_asin(x); },
    acos: function(x) { return __Math_acos(x); },
    atan: function(x) { return __Math_atan(x); },
    atan2: function(y, x) { return __Math_atan2(y, x); },

    // Hyperbolic
    sinh: function(x) { return __Math_sinh(x); },
    cosh: function(x) { return __Math_cosh(x); },
    tanh: function(x) { return __Math_tanh(x); },
    asinh: function(x) { return __Math_asinh(x); },
    acosh: function(x) { return __Math_acosh(x); },
    atanh: function(x) { return __Math_atanh(x); },

    // Min/Max/Random
    min: function(...values) { return __Math_min(...values); },
    max: function(...values) { return __Math_max(...values); },
    random: function() { return __Math_random(); },

    // Special
    clz32: function(x) { return __Math_clz32(x); },
    imul: function(a, b) { return __Math_imul(a, b); },
    fround: function(x) { return __Math_fround(x); },
    f16round: function(x) { return __Math_f16round(x); },
};

// String built-in wrapper
globalThis.String = function(value) {
    return value === undefined ? '' : String(value);
};

String.fromCharCode = function(...codes) {
    return __String_fromCharCode(...codes);
};

String.fromCodePoint = function(...codePoints) {
    return __String_fromCodePoint(...codePoints);
};

// String.prototype methods
String.prototype = {
    charAt: function(index) {
        return __String_charAt(this, index);
    },
    charCodeAt: function(index) {
        return __String_charCodeAt(this, index);
    },
    codePointAt: function(pos) {
        return __String_codePointAt(this, pos);
    },
    concat: function(...strings) {
        return __String_concat(this, ...strings);
    },
    includes: function(searchString, position) {
        return __String_includes(this, searchString, position);
    },
    indexOf: function(searchValue, fromIndex) {
        return __String_indexOf(this, searchValue, fromIndex);
    },
    lastIndexOf: function(searchValue, fromIndex) {
        return __String_lastIndexOf(this, searchValue, fromIndex);
    },
    slice: function(start, end) {
        return __String_slice(this, start, end);
    },
    substring: function(start, end) {
        return __String_substring(this, start, end);
    },
    split: function(separator, limit) {
        return __String_split(this, separator, limit);
    },
    toLowerCase: function() {
        return __String_toLowerCase(this);
    },
    toUpperCase: function() {
        return __String_toUpperCase(this);
    },
    toLocaleLowerCase: function(locales) {
        return __String_toLocaleLowerCase(this, locales);
    },
    toLocaleUpperCase: function(locales) {
        return __String_toLocaleUpperCase(this, locales);
    },
    trim: function() {
        return __String_trim(this);
    },
    trimStart: function() {
        return __String_trimStart(this);
    },
    trimEnd: function() {
        return __String_trimEnd(this);
    },
    trimLeft: function() {
        // Alias for trimStart
        return __String_trimStart(this);
    },
    trimRight: function() {
        // Alias for trimEnd
        return __String_trimEnd(this);
    },
    replace: function(searchValue, replaceValue) {
        return __String_replace(this, searchValue, replaceValue);
    },
    replaceAll: function(searchValue, replaceValue) {
        return __String_replaceAll(this, searchValue, replaceValue);
    },
    startsWith: function(searchString, position) {
        return __String_startsWith(this, searchString, position);
    },
    endsWith: function(searchString, endPosition) {
        return __String_endsWith(this, searchString, endPosition);
    },
    repeat: function(count) {
        return __String_repeat(this, count);
    },
    padStart: function(targetLength, padString) {
        return __String_padStart(this, targetLength, padString);
    },
    padEnd: function(targetLength, padString) {
        return __String_padEnd(this, targetLength, padString);
    },
    at: function(index) {
        return __String_at(this, index);
    },
    normalize: function(form) {
        return __String_normalize(this, form);
    },
    isWellFormed: function() {
        return __String_isWellFormed(this);
    },
    toWellFormed: function() {
        return __String_toWellFormed(this);
    },
    localeCompare: function(compareString, locales, options) {
        return __String_localeCompare(this, compareString, locales, options);
    },
    get length() {
        return __String_length(this);
    },
    toString: function() {
        return this;
    },
    valueOf: function() {
        return this;
    },
};

// Number built-in wrapper
globalThis.Number = function(value) {
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
Number.isFinite = function(value) {
    return __Number_isFinite(value);
};

Number.isInteger = function(value) {
    return __Number_isInteger(value);
};

Number.isNaN = function(value) {
    return __Number_isNaN(value);
};

Number.isSafeInteger = function(value) {
    return __Number_isSafeInteger(value);
};

Number.parseFloat = function(string) {
    return __Number_parseFloat(string);
};

Number.parseInt = function(string, radix) {
    return __Number_parseInt(string, radix);
};

// Number.prototype methods
Number.prototype = {
    toFixed: function(digits) {
        return __Number_toFixed(this, digits);
    },
    toExponential: function(fractionDigits) {
        return __Number_toExponential(this, fractionDigits);
    },
    toPrecision: function(precision) {
        return __Number_toPrecision(this, precision);
    },
    toString: function(radix) {
        return __Number_toString(this, radix);
    },
    toLocaleString: function(locales, options) {
        return __Number_toLocaleString(this, locales, options);
    },
    valueOf: function() {
        return __Number_valueOf(this);
    },
};

// Array.prototype methods (simplified - real impl needs prototype chain)
Array.prototype = {
    // === Mutating Methods ===
    push: function(...items) {
        return __Array_push(this, items);
    },
    pop: function() {
        return __Array_pop(this);
    },
    shift: function() {
        return __Array_shift(this);
    },
    unshift: function(...items) {
        return __Array_unshift(this, items);
    },
    splice: function(start, deleteCount, ...items) {
        return __Array_splice({
            arr: this,
            start: start,
            delete_count: deleteCount,
            items: items.length > 0 ? items : null
        });
    },
    reverse: function() {
        return __Array_reverse(this);
    },
    sort: function(compareFn) {
        // Note: compareFn not supported in JSON ops - lexicographic sort only
        return __Array_sort(this);
    },
    fill: function(value, start, end) {
        return __Array_fill({
            arr: this,
            value: value,
            start: start,
            end: end
        });
    },
    copyWithin: function(target, start, end) {
        return __Array_copyWithin({
            arr: this,
            target: target,
            start: start,
            end: end
        });
    },

    // === Non-Mutating Methods ===
    slice: function(start, end) {
        return __Array_slice({ arr: this, start: start, end: end });
    },
    concat: function(...items) {
        return __Array_concat(this, items);
    },
    flat: function(depth) {
        return __Array_flat({ arr: this, depth: depth });
    },
    flatMap: function(callback, thisArg) {
        // Execute callback in JS, pass results to native
        const mapped = [];
        for (let i = 0; i < this.length; i++) {
            mapped.push(callback.call(thisArg, this[i], i, this));
        }
        return __Array_flatMap({ arr: this, mapped: mapped });
    },

    // === Search Methods ===
    indexOf: function(searchElement, fromIndex) {
        return __Array_indexOf(this, searchElement);
    },
    lastIndexOf: function(searchElement, fromIndex) {
        return __Array_lastIndexOf(this, searchElement);
    },
    includes: function(searchElement, fromIndex) {
        return __Array_includes(this, searchElement);
    },
    find: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_find({ arr: this, results: results });
    },
    findIndex: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findIndex({ arr: this, results: results });
    },
    findLast: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findLast({ arr: this, results: results });
    },
    findLastIndex: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_findLastIndex({ arr: this, results: results });
    },
    at: function(index) {
        return __Array_at(this, index);
    },

    // === Iteration Methods ===
    forEach: function(callback, thisArg) {
        for (let i = 0; i < this.length; i++) {
            callback.call(thisArg, this[i], i, this);
        }
        return __Array_forEach(this);
    },
    map: function(callback, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(callback.call(thisArg, this[i], i, this));
        }
        return __Array_map({ results: results });
    },
    filter: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_filter({ arr: this, results: results });
    },
    reduce: function(callback, initialValue) {
        let accumulator = initialValue !== undefined ? initialValue : this[0];
        const startIndex = initialValue !== undefined ? 0 : 1;
        for (let i = startIndex; i < this.length; i++) {
            accumulator = callback(accumulator, this[i], i, this);
        }
        return __Array_reduce({ result: accumulator });
    },
    reduceRight: function(callback, initialValue) {
        let accumulator = initialValue !== undefined ? initialValue : this[this.length - 1];
        const startIndex = initialValue !== undefined ? this.length - 1 : this.length - 2;
        for (let i = startIndex; i >= 0; i--) {
            accumulator = callback(accumulator, this[i], i, this);
        }
        return __Array_reduceRight({ result: accumulator });
    },
    every: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_every(results);
    },
    some: function(predicate, thisArg) {
        const results = [];
        for (let i = 0; i < this.length; i++) {
            results.push(!!predicate.call(thisArg, this[i], i, this));
        }
        return __Array_some(results);
    },

    // === Conversion Methods ===
    join: function(separator) {
        return __Array_join(this, separator);
    },
    toString: function() {
        return __Array_toString(this);
    },
    get length() {
        return __Array_length(this);
    },

    // === ES2023 Immutable Methods ===
    toReversed: function() {
        return __Array_toReversed(this);
    },
    toSorted: function(compareFn) {
        // Note: compareFn not supported in JSON ops - lexicographic sort only
        return __Array_toSorted(this);
    },
    toSpliced: function(start, deleteCount, ...items) {
        return __Array_toSpliced({
            arr: this,
            start: start,
            delete_count: deleteCount,
            items: items.length > 0 ? items : null
        });
    },
    with: function(index, value) {
        return __Array_with({
            arr: this,
            index: index,
            value: value
        });
    },
};

// Console built-in
globalThis.console = {
    log: function(...args) { return __console_log(...args); },
    error: function(...args) { return __console_error(...args); },
    warn: function(...args) { return __console_warn(...args); },
    info: function(...args) { return __console_info(...args); },
    debug: function(...args) { return __console_debug(...args); },
    trace: function(...args) { return __console_trace(...args); },
    time: function(label) { return __console_time(label); },
    timeEnd: function(label) { return __console_timeEnd(label); },
    timeLog: function(label, ...args) { return __console_timeLog(label, ...args); },
    assert: function(condition, ...args) { return __console_assert(condition, ...args); },
    clear: function() { return __console_clear(); },
    count: function(label) { return __console_count(label); },
    countReset: function(label) { return __console_countReset(label); },
    table: function(data, columns) { return __console_table(data, columns); },
    dir: function(obj, options) { return __console_dir(obj, options); },
    dirxml: function(...args) { return __console_dirxml(...args); },
    // Group methods (simplified - just log for now)
    group: function(...args) { return __console_log(...args); },
    groupCollapsed: function(...args) { return __console_log(...args); },
    groupEnd: function() {},
};
