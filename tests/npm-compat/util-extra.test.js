const {
  formatWithOptions,
  debuglog,
  parseArgs,
  MIMEType,
  MIMEParams,
} = require('node:util');

const tests = [
  {
    name: 'formatWithOptions respects inspect depth',
    fn: () =>
      formatWithOptions({ depth: 0 }, '%O', {
        nested: { value: 1 },
      }),
    expect: '{ nested: [Object] }',
  },
  {
    name: 'debuglog respects NODE_DEBUG',
    fn: () => {
      const originalEnv = process.env.NODE_DEBUG;
      process.env.NODE_DEBUG = 'UTILTEST';
      const originalError = console.error;
      const logs = [];
      console.error = (...args) => {
        logs.push(args.join(' '));
      };
      try {
        const logger = debuglog('utiltest');
        logger('ping', { ok: true });
      } finally {
        console.error = originalError;
        if (originalEnv !== undefined) {
          process.env.NODE_DEBUG = originalEnv;
        } else {
          delete process.env.NODE_DEBUG;
        }
      }
      return logs;
    },
    expect: ['[UTILTEST] ping { ok: true }'],
  },
  {
    name: 'parseArgs exposes tokens with inline values',
    fn: () =>
      parseArgs({
        options: { foo: { type: 'string' } },
        args: ['--foo=bar', 'extra'],
        allowPositionals: true,
        tokens: true,
      }),
    expect: {
      values: { foo: 'bar' },
      positionals: ['extra'],
      tokens: [
        {
          kind: 'option',
          name: 'foo',
          rawName: '--foo',
          value: 'bar',
          inlineValue: true,
          index: 0,
        },
        { kind: 'positional', value: 'extra', index: 1 },
      ],
    },
  },
  {
    name: 'MIMEType parses params and snapshots',
    fn: () => {
      const mime = new MIMEType('text/html; charset=UTF-8');
      return {
        type: mime.type,
        subtype: mime.subtype,
        essence: mime.essence,
        params: [...mime.params.entries()],
        paramsToString: mime.params.toString(),
        toString: mime.toString(),
        paramsJson: JSON.stringify(mime.params),
        paramsProto: mime.params instanceof MIMEParams,
      };
    },
    expect: {
      type: 'text',
      subtype: 'html',
      essence: 'text/html',
      params: [['charset', 'UTF-8']],
      paramsToString: 'charset=UTF-8',
      toString: 'text/html;charset=UTF-8',
      paramsJson: JSON.stringify('charset=UTF-8'),
      paramsProto: true,
    },
  },
];

let passed = 0;
let failed = 0;
const failures = [];

console.log('=== util Extra JS Tests ===\n');

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
