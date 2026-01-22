// Test262 assert harness

function assert(condition, message) {
    if (!condition) {
        throw new Error(message || "Assertion failed");
    }
}

assert.sameValue = function(actual, expected, message) {
    if (actual !== expected) {
        var msg = message ? message + " " : "";
        throw new Error(msg + "Expected " + String(expected) + ", got " + String(actual));
    }
};

assert.notSameValue = function(actual, unexpected, message) {
    if (actual === unexpected) {
        var msg = message ? message + " " : "";
        throw new Error(msg + "Unexpected value: " + String(actual));
    }
};

assert.throws = function(errorType, fn, message) {
    var thrown = false;
    var thrownError = null;

    try {
        fn();
    } catch (e) {
        thrown = true;
        thrownError = e;
    }

    if (!thrown) {
        throw new Error(message || "Expected exception to be thrown");
    }

    if (errorType && !(thrownError instanceof errorType)) {
        throw new Error(message || "Expected " + errorType.name + ", got " + thrownError.constructor.name);
    }
};
