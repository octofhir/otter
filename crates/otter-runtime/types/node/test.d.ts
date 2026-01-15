/**
 * The `node:test` module provides a test runner with describe, it, and test functions.
 * @module node:test
 */
declare module "node:test" {
    /**
     * Create a test suite. Tests and nested suites can be defined inside the callback.
     * @param name The name of the test suite
     * @param fn Callback containing test definitions
     * @example
     * describe('Math', () => {
     *     it('adds numbers', () => {
     *         assert.equal(1 + 1, 2);
     *     });
     * });
     */
    export function describe(name: string, fn: () => void | Promise<void>): void;

    /**
     * Define a test case.
     * @param name The name of the test
     * @param fn The test function (can be async)
     * @example
     * it('should work', () => {
     *     assert.equal(1, 1);
     * });
     */
    export function it(name: string, fn: () => void | Promise<void>): void;

    /**
     * Alias for `it()`. Define a test case.
     * @param name The name of the test
     * @param fn The test function (can be async)
     */
    export function test(name: string, fn: () => void | Promise<void>): void;

    export namespace it {
        /**
         * Skip this test. The test will be marked as skipped in results.
         * @param name The name of the test
         * @param fn The test function (not executed)
         */
        function skip(name: string, fn: () => void | Promise<void>): void;

        /**
         * Run only this test (and other .only tests). All other tests will be skipped.
         * @param name The name of the test
         * @param fn The test function
         */
        function only(name: string, fn: () => void | Promise<void>): void;
    }

    export namespace test {
        /**
         * Skip this test.
         */
        function skip(name: string, fn: () => void | Promise<void>): void;

        /**
         * Run only this test.
         */
        function only(name: string, fn: () => void | Promise<void>): void;
    }

    /**
     * Register a function to run before each test in the current suite.
     * @param fn Setup function to run before each test
     * @example
     * describe('Array', () => {
     *     let arr: number[];
     *     beforeEach(() => {
     *         arr = [1, 2, 3];
     *     });
     *     it('has length', () => {
     *         assert.equal(arr.length, 3);
     *     });
     * });
     */
    export function beforeEach(fn: () => void | Promise<void>): void;

    /**
     * Register a function to run after each test in the current suite.
     * @param fn Teardown function to run after each test
     */
    export function afterEach(fn: () => void | Promise<void>): void;

    /**
     * Register a function to run once before all tests in the current suite.
     * @param fn Setup function to run once before all tests
     */
    export function before(fn: () => void | Promise<void>): void;

    /**
     * Register a function to run once after all tests in the current suite.
     * @param fn Teardown function to run once after all tests
     */
    export function after(fn: () => void | Promise<void>): void;

    /**
     * Run all queued tests and return a summary.
     * @returns Promise resolving to test summary
     */
    export function run(): Promise<TestSummary>;

    /**
     * Assertion utilities for testing.
     */
    export const assert: {
        /**
         * Assert that actual equals expected (using ==).
         * @param actual The actual value
         * @param expected The expected value
         * @throws If values are not equal
         */
        equal<T>(actual: T, expected: T): void;

        /**
         * Assert that actual strictly equals expected (using ===).
         * @param actual The actual value
         * @param expected The expected value
         * @throws If values are not strictly equal
         */
        strictEqual<T>(actual: T, expected: T): void;

        /**
         * Assert that actual does not equal expected.
         * @param actual The actual value
         * @param expected The value it should not equal
         * @throws If values are equal
         */
        notEqual<T>(actual: T, expected: T): void;

        /**
         * Assert that value is truthy.
         * @param value The value to check
         * @throws If value is falsy
         */
        ok(value: unknown): void;

        /**
         * Assert that value is true.
         * @param value The value to check
         * @throws If value is not true
         */
        true(value: boolean): void;

        /**
         * Assert that value is false.
         * @param value The value to check
         * @throws If value is not false
         */
        false(value: boolean): void;

        /**
         * Assert that the function throws an error.
         * @param fn Function expected to throw
         * @param expected Optional expected error message substring
         * @throws If function does not throw (or throws wrong error)
         */
        throws(fn: () => void | Promise<void>, expected?: string | RegExp): Promise<void>;

        /**
         * Assert deep equality between actual and expected.
         * @param actual The actual value
         * @param expected The expected value
         * @throws If values are not deeply equal
         */
        deepEqual<T>(actual: T, expected: T): void;

        /**
         * Assert that actual is null or undefined.
         * @param actual The value to check
         * @throws If value is not null/undefined
         */
        isNull(actual: unknown): void;

        /**
         * Assert that actual is not null or undefined.
         * @param actual The value to check
         * @throws If value is null/undefined
         */
        isNotNull(actual: unknown): void;
    };

    /**
     * Summary of test execution results.
     */
    export interface TestSummary {
        /** Number of passed tests */
        passed: number;
        /** Number of failed tests */
        failed: number;
        /** Number of skipped tests */
        skipped: number;
        /** Total number of tests */
        total: number;
        /** Detailed results for each test */
        results: TestResult[];
    }

    /**
     * Result of a single test execution.
     */
    export interface TestResult {
        /** Full name of the test including suite path */
        name: string;
        /** Whether the test passed */
        passed: boolean;
        /** Duration in milliseconds */
        duration: number;
        /** Error message if test failed */
        error?: string;
        /** Whether the test was skipped */
        skipped?: boolean;
    }
}

// Also support the 'test' module (without node: prefix)
declare module "test" {
    export * from "node:test";
}
