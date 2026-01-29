// Test262 standard test assertions

var $ERROR = function(message) {
    throw new Error(message);
};

function Test262Error(message) {
    this.message = message || "";
}
Test262Error.prototype.toString = function() {
    return "Test262Error: " + this.message;
};

var $DONE = function(error) {
    if (error) {
        if (typeof error === "object" && error !== null && "message" in error) {
            __test262_done(false, error.message);
        } else {
            __test262_done(false, String(error));
        }
    } else {
        __test262_done(true, "");
    }
};

// SameValue algorithm (ES6 7.2.9)
// https://tc39.es/ecma262/#sec-samevalue
function isSameValue(x, y) {
    if (typeof x !== typeof y) {
        return false;
    }
    if (typeof x === 'number') {
        // Handle NaN
        if (isNaN(x) && isNaN(y)) {
            return true;
        }
        // Handle +0 vs -0
        if (x === 0 && y === 0) {
            return 1/x === 1/y;
        }
        return x === y;
    }
    return x === y;
}

// SameValueZero algorithm (ES6 7.2.10)
// Like SameValue but treats +0 and -0 as equal
function isSameValueZero(x, y) {
    if (typeof x !== typeof y) {
        return false;
    }
    if (typeof x === 'number') {
        // Handle NaN
        if (isNaN(x) && isNaN(y)) {
            return true;
        }
        return x === y;
    }
    return x === y;
}

// Check if value can be used as a constructor
function isConstructor(fn) {
    if (typeof fn !== 'function') {
        return false;
    }
    // Check if function has [[Construct]] internal method
    // In practice, check if it can be called with 'new'
    try {
        // Arrow functions and some built-ins throw when called with new
        if (fn.prototype === undefined) {
            return false;
        }
        // Try to access prototype property (constructors should have it)
        return typeof fn.prototype === 'object' || typeof fn.prototype === 'function';
    } catch (e) {
        return false;
    }
}

// Compare two values including their types
function compareArray(a, b) {
    if (!Array.isArray(a) || !Array.isArray(b)) {
        return false;
    }
    if (a.length !== b.length) {
        return false;
    }
    for (var i = 0; i < a.length; i++) {
        if (!isSameValue(a[i], b[i])) {
            return false;
        }
    }
    return true;
}

// Helper to check if value is an array of specific type
function isArrayOfType(arr, checkFn) {
    if (!Array.isArray(arr)) {
        return false;
    }
    for (var i = 0; i < arr.length; i++) {
        if (!checkFn(arr[i])) {
            return false;
        }
    }
    return true;
}
