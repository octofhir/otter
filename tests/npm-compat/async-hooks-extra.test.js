const { AsyncLocalStorage } = require('node:async_hooks');

const tests = [
  {
    name: 'AsyncLocalStorage.run preserves store across setImmediate',
    fn: () =>
      new Promise((resolve) => {
        const als = new AsyncLocalStorage();
        als.run({ id: 'immediate' }, () => {
          setImmediate(() => resolve(als.getStore()));
        });
      }),
    expect: { id: 'immediate' },
  },
  {
    name: 'AsyncLocalStorage.enterWith keeps store in current sync context',
    fn: () => {
      const als = new AsyncLocalStorage();
      als.enterWith({ id: 'enter' });
      return als.getStore();
    },
    expect: { id: 'enter' },
  },
  {
    name: 'AsyncLocalStorage.exit temporarily suspends the store',
    fn: () => {
      const als = new AsyncLocalStorage();
      let inside;
      als.run({ id: 'outer' }, () => {
        als.exit(() => {
          inside = als.getStore();
        });
      });
      return inside;
    },
    expect: undefined,
  },
];

let passed = 0;
let failed = 0;
const failures = [];

console.log('=== async_hooks Extra JS Tests ===\n');

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
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
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
    for (const failure of failures) {
      console.log(`  - ${failure}`);
    }
  }

  if (failed > 0) {
    process.exit(1);
  }
}

runTests();
