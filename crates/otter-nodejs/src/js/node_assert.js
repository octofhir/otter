// Node.js assert module - ESM export wrapper

class AssertionError extends Error {
    constructor(options) {
        super(options.message || 'Assertion failed');
        this.name = 'AssertionError';
        this.actual = options.actual;
        this.expected = options.expected;
        this.operator = options.operator;
    }
}

function assert(value, message) {
    if (!value) {
        throw new AssertionError({ message: message || 'The expression evaluated to a falsy value', actual: value, expected: true, operator: '==' });
    }
}

assert.ok = assert;

assert.equal = function (actual, expected, message) {
    if (actual != expected) {
        throw new AssertionError({ message, actual, expected, operator: '==' });
    }
};

assert.notEqual = function (actual, expected, message) {
    if (actual == expected) {
        throw new AssertionError({ message, actual, expected, operator: '!=' });
    }
};

assert.strictEqual = function (actual, expected, message) {
    if (actual !== expected) {
        throw new AssertionError({ message, actual, expected, operator: '===' });
    }
};

assert.notStrictEqual = function (actual, expected, message) {
    if (actual === expected) {
        throw new AssertionError({ message, actual, expected, operator: '!==' });
    }
};

assert.deepEqual = function (actual, expected, message) {
    if (JSON.stringify(actual) !== JSON.stringify(expected)) {
        throw new AssertionError({ message, actual, expected, operator: 'deepEqual' });
    }
};

assert.deepStrictEqual = assert.deepEqual;

assert.throws = function (fn, expected, message) {
    let threw = false;
    try {
        fn();
    } catch (e) {
        threw = true;
        if (expected && !(e instanceof expected)) {
            throw new AssertionError({ message: message || 'Wrong error type', actual: e, expected, operator: 'throws' });
        }
    }
    if (!threw) {
        throw new AssertionError({ message: message || 'Expected function to throw', operator: 'throws' });
    }
};

assert.doesNotThrow = function (fn, message) {
    try {
        fn();
    } catch (e) {
        throw new AssertionError({ message: message || 'Got unwanted exception', actual: e, operator: 'doesNotThrow' });
    }
};

assert.fail = function (message) {
    throw new AssertionError({ message: message || 'Failed', operator: 'fail' });
};

assert.AssertionError = AssertionError;

export { assert as default, AssertionError };
export const ok = assert.ok;
export const equal = assert.equal;
export const notEqual = assert.notEqual;
export const strictEqual = assert.strictEqual;
export const notStrictEqual = assert.notStrictEqual;
export const deepEqual = assert.deepEqual;
export const deepStrictEqual = assert.deepStrictEqual;
export const throws = assert.throws;
export const doesNotThrow = assert.doesNotThrow;
export const fail = assert.fail;
