// node:test wrapper - provides describe, it, test, and assertion APIs

(function() {
    'use strict';

    // Test queue and state
    const testQueue = [];
    let currentSuite = null;
    let hasOnly = false;

    // describe - create a test suite
    globalThis.describe = function describe(name, fn) {
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            beforeAll: null,
            afterAll: null,
            beforeEach: null,
            afterEach: null
        };

        const prevSuite = currentSuite;
        currentSuite = suite;

        // Execute the callback to collect tests
        fn();

        currentSuite = prevSuite;

        if (prevSuite) {
            prevSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // it / test - define a test
    globalThis.it = function it(name, fn) {
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: false,
            only: false
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test = globalThis.it;

    // it.skip - skip a test
    globalThis.it.skip = function skip(name, fn) {
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: true,
            only: false
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test.skip = globalThis.it.skip;

    // it.only - run only this test
    globalThis.it.only = function only(name, fn) {
        hasOnly = true;
        const testCase = {
            type: 'test',
            name: name,
            fn: fn,
            skip: false,
            only: true
        };

        if (currentSuite) {
            currentSuite.tests.push(testCase);
        } else {
            testQueue.push(testCase);
        }
    };

    globalThis.test.only = globalThis.it.only;

    // describe.skip - skip a suite
    globalThis.describe.skip = function skip(name, fn) {
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            skip: true
        };
        if (currentSuite) {
            currentSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // describe.only - run only this suite
    globalThis.describe.only = function only(name, fn) {
        hasOnly = true;
        const suite = {
            type: 'suite',
            name: name,
            tests: [],
            only: true,
            beforeAll: null,
            afterAll: null,
            beforeEach: null,
            afterEach: null
        };

        const prevSuite = currentSuite;
        currentSuite = suite;
        fn();
        currentSuite = prevSuite;

        if (prevSuite) {
            prevSuite.tests.push(suite);
        } else {
            testQueue.push(suite);
        }
    };

    // Hook functions
    globalThis.beforeEach = function beforeEach(fn) {
        if (currentSuite) {
            currentSuite.beforeEach = fn;
        }
    };

    globalThis.afterEach = function afterEach(fn) {
        if (currentSuite) {
            currentSuite.afterEach = fn;
        }
    };

    globalThis.before = function before(fn) {
        if (currentSuite) {
            currentSuite.beforeAll = fn;
        }
    };

    globalThis.after = function after(fn) {
        if (currentSuite) {
            currentSuite.afterAll = fn;
        }
    };

    // Check if any test in the tree has .only
    function checkHasOnly(items) {
        for (const item of items) {
            if (item.only) return true;
            if (item.type === 'suite' && item.tests) {
                if (checkHasOnly(item.tests)) return true;
            }
        }
        return false;
    }

    // Run a single test (async version - supports async test functions)
    async function runTest(test, suitePath, hooks) {
        const fullName = suitePath ? suitePath + ' > ' + test.name : test.name;

        // Check if should skip
        if (test.skip) {
            __skipTest(test.name);
            console.log('  - ' + fullName + ' (skipped)');
            return;
        }

        // Check if we have .only tests and this isn't one
        if (hasOnly && !test.only) {
            __skipTest(test.name);
            console.log('  - ' + fullName + ' (skipped)');
            return;
        }

        const start = Date.now();
        __startSuite(test.name);

        try {
            // Run beforeEach hooks
            if (hooks.beforeEach) {
                await hooks.beforeEach();
            }

            // Run the test (await in case it's async)
            await test.fn();

            // Run afterEach hooks
            if (hooks.afterEach) {
                await hooks.afterEach();
            }

            const duration = Date.now() - start;
            __recordResult(test.name, true, duration, null);
            console.log('  ✓ ' + fullName + ' (' + duration + 'ms)');
        } catch (error) {
            const duration = Date.now() - start;
            const errorMsg = error && error.message ? error.message : String(error);
            __recordResult(test.name, false, duration, errorMsg);
            console.log('  ✗ ' + fullName + ' (' + duration + 'ms)');
            console.log('    ' + errorMsg);
        }

        __endSuite();
    }

    // Run a test suite (async version)
    async function runSuite(suite, parentPath, parentHooks) {
        const suitePath = parentPath ? parentPath + ' > ' + suite.name : suite.name;

        // Check if should skip entire suite
        if (suite.skip) {
            console.log('\n' + suitePath + ' (skipped)');
            for (const item of (suite.tests || [])) {
                if (item.type === 'test') {
                    __skipTest(item.name);
                }
            }
            return;
        }

        console.log('\n' + suitePath);
        __startSuite(suite.name);

        // Merge hooks
        const hooks = {
            beforeEach: suite.beforeEach || parentHooks.beforeEach,
            afterEach: suite.afterEach || parentHooks.afterEach
        };

        // Run beforeAll
        if (suite.beforeAll) {
            try {
                await suite.beforeAll();
            } catch (error) {
                console.log('  beforeAll failed: ' + (error.message || error));
            }
        }

        // Run tests and nested suites
        for (const item of (suite.tests || [])) {
            if (item.type === 'suite') {
                await runSuite(item, suitePath, hooks);
            } else {
                await runTest(item, suitePath, hooks);
            }
        }

        // Run afterAll
        if (suite.afterAll) {
            try {
                await suite.afterAll();
            } catch (error) {
                console.log('  afterAll failed: ' + (error.message || error));
            }
        }

        __endSuite();
    }

    // run - execute all queued tests (async version)
    globalThis.run = async function run() {
        // Reset runner state
        __resetTests();

        // Check for .only tests
        hasOnly = checkHasOnly(testQueue);

        console.log('Running tests...');

        for (const item of testQueue) {
            if (item.type === 'suite') {
                await runSuite(item, '', {});
            } else {
                await runTest(item, '', {});
            }
        }

        const summary = __getSummary();

        console.log('\n' + summary.passed + ' passing');
        if (summary.failed > 0) {
            console.log(summary.failed + ' failing');
        }
        if (summary.skipped > 0) {
            console.log(summary.skipped + ' skipped');
        }

        // Clear queue for next run
        testQueue.length = 0;
        hasOnly = false;

        return summary;
    };

    // assert - assertion utilities
    globalThis.assert = {
        equal: function(actual, expected) {
            assertEqual(actual, expected);
        },
        strictEqual: function(actual, expected) {
            assertEqual(actual, expected);
        },
        notEqual: function(actual, expected) {
            assertNotEqual(actual, expected);
        },
        ok: function(value) {
            assertOk(value);
        },
        true: function(value) {
            assertTrue(value);
        },
        false: function(value) {
            assertFalse(value);
        },
        deepEqual: function(actual, expected) {
            assertDeepEqual(actual, expected);
        },
        throws: async function(fn, expected) {
            let threw = false;
            let error = null;
            try {
                await fn();
            } catch (e) {
                threw = true;
                error = e;
            }
            if (!threw) {
                throw new Error('Expected function to throw');
            }
            if (expected) {
                const msg = error && error.message ? error.message : String(error);
                if (typeof expected === 'string' && !msg.includes(expected)) {
                    throw new Error('Expected error "' + expected + '", got "' + msg + '"');
                }
                if (expected instanceof RegExp && !expected.test(msg)) {
                    throw new Error('Expected error matching ' + expected + ', got "' + msg + '"');
                }
            }
        },
        isNull: function(value) {
            if (value !== null && value !== undefined) {
                throw new Error('Expected null or undefined, got ' + typeof value);
            }
        },
        isNotNull: function(value) {
            if (value === null || value === undefined) {
                throw new Error('Expected non-null value');
            }
        }
    };

    const testModule = {
        describe: globalThis.describe,
        it: globalThis.it,
        test: globalThis.test,
        run: globalThis.run,
        assert: globalThis.assert,
    };
    testModule.default = testModule;

    if (globalThis.__registerNodeBuiltin) {
        globalThis.__registerNodeBuiltin('test', testModule);
    }
})();
