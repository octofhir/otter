/**
 * async_hooks compatibility tests for Otter runtime
 * Tests the core node:async_hooks API surface and basic behavior
 */

import {
  AsyncLocalStorage,
  AsyncResource,
  createHook,
  executionAsyncId,
  executionAsyncResource,
  triggerAsyncId,
} from 'node:async_hooks';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  {
    name: 'async_hooks exports exist',
    fn: () => ({
      executionAsyncId: typeof executionAsyncId,
      triggerAsyncId: typeof triggerAsyncId,
      executionAsyncResource: typeof executionAsyncResource,
      AsyncResource: typeof AsyncResource,
      AsyncLocalStorage: typeof AsyncLocalStorage,
      createHook: typeof createHook,
    }),
    expect: {
      executionAsyncId: 'function',
      triggerAsyncId: 'function',
      executionAsyncResource: 'function',
      AsyncResource: 'function',
      AsyncLocalStorage: 'function',
      createHook: 'function',
    },
  },
  {
    name: 'executionAsyncId returns a number',
    fn: () => typeof executionAsyncId(),
    expect: 'number',
  },
  {
    name: 'triggerAsyncId returns a number',
    fn: () => typeof triggerAsyncId(),
    expect: 'number',
  },
  {
    name: 'executionAsyncResource tracks AsyncResource scope',
    fn: () => {
      const resource = new AsyncResource('test');
      let matches = false;
      resource.runInAsyncScope(() => {
        matches = executionAsyncResource() === resource;
      });
      return matches;
    },
    expect: true,
  },
  {
    name: 'createHook receives init/before/after/destroy events',
    fn: () => {
      const events: string[] = [];
      const hook = createHook({
        init() {
          events.push('init');
        },
        before() {
          events.push('before');
        },
        after() {
          events.push('after');
        },
        destroy() {
          events.push('destroy');
        },
      }).enable();

      const resource = new AsyncResource('hook-test');
      resource.runInAsyncScope(() => {});
      resource.emitDestroy();
      hook.disable();

      return events;
    },
    expect: ['init', 'before', 'after', 'destroy'],
  },
  {
    name: 'AsyncLocalStorage.run sets and restores store',
    fn: () => {
      const als = new AsyncLocalStorage();
      let storeValue: unknown;
      als.run({ user: 'alice' }, () => {
        storeValue = als.getStore();
      });
      return storeValue;
    },
    expect: { user: 'alice' },
  },
  {
    name: 'AsyncLocalStorage.enterWith and exit work',
    fn: () => {
      const als = new AsyncLocalStorage();
      als.enterWith({ request: 1 });
      const inside = als.getStore();
      const exitValue = als.exit(() => als.getStore());
      const after = als.getStore();
      return { inside, exitValue, after };
    },
    expect: {
      inside: { request: 1 },
      exitValue: undefined,
      after: { request: 1 },
    },
  },
  {
    name: 'AsyncLocalStorage.bind captures store',
    fn: () => {
      const als = new AsyncLocalStorage();
      let bound: (() => unknown) | undefined;
      als.run({ id: 7 }, () => {
        bound = AsyncLocalStorage.bind(() => als.getStore());
      });
      return bound ? bound() : null;
    },
    expect: { id: 7 },
  },
  {
    name: 'AsyncLocalStorage.snapshot captures store',
    fn: () => {
      const als = new AsyncLocalStorage();
      let snapshot: ((fn: () => unknown) => unknown) | undefined;
      als.run({ id: 9 }, () => {
        snapshot = AsyncLocalStorage.snapshot();
      });
      return snapshot ? snapshot(() => als.getStore()) : null;
    },
    expect: { id: 9 },
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== async_hooks Compatibility Tests ===\n');

async function runTests() {
  for (const test of tests) {
    try {
      let result = test.fn();
      if (result instanceof Promise) {
        result = await result;
      }
      const resultStr = JSON.stringify(result);
      const expectStr = JSON.stringify(test.expect);

      if (resultStr === expectStr) {
        console.log(`PASS: ${test.name}`);
        passed++;
      } else {
        console.log(`FAIL: ${test.name}`);
        console.log(`  Expected: ${expectStr}`);
        console.log(`  Got: ${resultStr}`);
        failed++;
        failures.push(`${test.name}: expected ${expectStr}, got ${resultStr}`);
      }
    } catch (e: unknown) {
      const msg = e instanceof Error ? e.message : String(e);
      console.log(`ERROR: ${test.name} - ${msg}`);
      failed++;
      failures.push(`${test.name}: ${msg}`);
    }
  }

  console.log('\n=== Summary ===');
  console.log(`Passed: ${passed}/${tests.length}`);
  console.log(`Failed: ${failed}/${tests.length}`);

  if (failures.length > 0) {
    console.log('\nFailures:');
    for (const f of failures) {
      console.log(`  - ${f}`);
    }
  }

  if (failed > 0) {
    process.exit(1);
  }
}

runTests();
