/**
 * The `node:assert` module provides assertion testing functions.
 * @module node:assert
 */
declare module "node:assert" {
    namespace assert {
        /**
         * Tests if value is truthy.
         */
        function ok(value: unknown, message?: string | Error): asserts value;

        /**
         * Tests strict equality using ===.
         */
        function strictEqual<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;

        /**
         * Tests strict inequality using !==.
         */
        function notStrictEqual(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Tests shallow equality using ==.
         */
        function equal(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Tests shallow inequality using !=.
         */
        function notEqual(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Tests deep equality.
         */
        function deepEqual(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Tests deep inequality.
         */
        function notDeepEqual(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Tests deep strict equality.
         */
        function deepStrictEqual<T>(actual: unknown, expected: T, message?: string | Error): asserts actual is T;

        /**
         * Tests deep strict inequality.
         */
        function notDeepStrictEqual(actual: unknown, expected: unknown, message?: string | Error): void;

        /**
         * Throws an AssertionError.
         */
        function fail(message?: string | Error): never;

        /**
         * Expects the function fn to throw an error.
         */
        function throws(fn: () => unknown, message?: string | Error): void;
        function throws(fn: () => unknown, error: RegExp | Function | object, message?: string | Error): void;

        /**
         * Expects the async function fn to reject.
         */
        function rejects(fn: () => Promise<unknown>, message?: string | Error): Promise<void>;
        function rejects(fn: () => Promise<unknown>, error: RegExp | Function | object, message?: string | Error): Promise<void>;

        /**
         * Expects the function fn not to throw.
         */
        function doesNotThrow(fn: () => unknown, message?: string | Error): void;

        /**
         * Expects the async function fn not to reject.
         */
        function doesNotReject(fn: () => Promise<unknown>, message?: string | Error): Promise<void>;

        /**
         * Tests if value matches the regular expression.
         */
        function match(value: string, regexp: RegExp, message?: string | Error): void;

        /**
         * Tests if value does not match the regular expression.
         */
        function doesNotMatch(value: string, regexp: RegExp, message?: string | Error): void;
    }

    /**
     * Tests if value is truthy. Alias for assert.ok().
     */
    function assert(value: unknown, message?: string | Error): asserts value;

    export = assert;
}

// Also support the 'assert' module (without node: prefix)
declare module "assert" {
    export = require("node:assert");
}
