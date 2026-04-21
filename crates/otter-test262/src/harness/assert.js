// Test262 assert harness

function assert(condition, message) {
    if (!condition) {
        throw new Error(message || "Assertion failed");
    }
}

// Use isSameValue for strict equality (handles NaN, +0/-0)
assert.sameValue = function(actual, expected, message) {
    if (typeof isSameValue === 'function' && !isSameValue(actual, expected)) {
        var msg = message ? message + " " : "";
        throw new Test262Error(msg + "Expected SameValue(«" + String(expected) + "», «" + String(actual) + "») to be true");
    } else if (actual !== expected) {
        var msg = message ? message + " " : "";
        throw new Test262Error(msg + "Expected " + String(expected) + ", got " + String(actual));
    }
};

assert.notSameValue = function(actual, unexpected, message) {
    if (typeof isSameValue === 'function' && isSameValue(actual, unexpected)) {
        var msg = message ? message + " " : "";
        throw new Test262Error(msg + "Unexpected value: " + String(actual));
    } else if (actual === unexpected) {
        var msg = message ? message + " " : "";
        throw new Test262Error(msg + "Unexpected value: " + String(actual));
    }
};

assert.throws = function(errorType, fn, message) {
    if (typeof errorType === 'function' && typeof fn !== 'function') {
        // Legacy single-argument form: assert.throws(fn)
        fn = errorType;
        errorType = undefined;
    }

    var thrown = false;
    var thrownError = null;

    try {
        fn();
    } catch (e) {
        thrown = true;
        thrownError = e;
    }

    if (!thrown) {
        var msg = message || "Expected exception to be thrown";
        if (errorType) {
            msg = "Expected a " + (errorType.name || errorType) + " to be thrown but no exception was thrown at all";
        }
        throw new Test262Error(msg);
    }

    if (errorType) {
        // Check if error is instance of expected type
        var isCorrectType = false;
        if (typeof errorType === 'function') {
            isCorrectType = thrownError instanceof errorType;
        } else if (typeof errorType === 'object' && errorType !== null) {
            // Constructor reference passed as object
            isCorrectType = thrownError.constructor === errorType.constructor ||
                           thrownError instanceof errorType.constructor;
        }

        if (!isCorrectType) {
            var expectedName = errorType.name || String(errorType);
            var actualName = thrownError.constructor ? thrownError.constructor.name : typeof thrownError;
            throw new Test262Error(
                (message || "") +
                "Expected a " + expectedName +
                " but got a " + actualName
            );
        }
    }
};

// Compare arrays element-by-element using SameValue
assert.compareArray = function(actual, expected, message) {
    if (!Array.isArray(actual) || !Array.isArray(expected)) {
        throw new Test262Error((message || "") + " compareArray requires both arguments to be arrays");
    }

    if (actual.length !== expected.length) {
        throw new Test262Error(
            (message || "") +
            " Expected array length " + expected.length +
            " but got " + actual.length
        );
    }

    for (var i = 0; i < actual.length; i++) {
        if (typeof isSameValue === 'function') {
            if (!isSameValue(actual[i], expected[i])) {
                throw new Test262Error(
                    (message || "") +
                    " Expected SameValue at index " + i + ": " +
                    "expected " + String(expected[i]) +
                    ", got " + String(actual[i])
                );
            }
        } else if (actual[i] !== expected[i]) {
            throw new Test262Error(
                (message || "") +
                " Expected value at index " + i + ": " +
                "expected " + String(expected[i]) +
                ", got " + String(actual[i])
            );
        }
    }
};
