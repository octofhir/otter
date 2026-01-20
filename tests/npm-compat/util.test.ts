/**
 * util compatibility tests for Otter runtime
 * Verifies extended util APIs (formatWithOptions, debuglog, parseArgs, MIMEType)
 */

import {
  formatWithOptions,
  debuglog,
  parseArgs,
  MIMEType,
  MIMEParams,
} from 'node:util';

interface TestCase {
  name: string;
  fn: () => unknown | Promise<unknown>;
  expect: unknown;
}

const tests: TestCase[] = [
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
      const logs: string[] = [];
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
    name: 'parseArgs handles string and boolean options',
    fn: () => {
      return parseArgs({
        options: {
          foo: { type: 'string', short: 'f' },
          bar: { type: 'boolean', short: 'b' },
        },
        args: ['--foo', 'value', '-b'],
      }).values;
    },
    expect: { foo: 'value', bar: true },
  },
  {
    name: 'parseArgs exposes tokens with inline values',
    fn: () => {
      return parseArgs({
        options: { foo: { type: 'string' } },
        args: ['--foo=bar', 'extra'],
        allowPositionals: true,
        tokens: true,
      });
    },
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
    name: 'parseArgs accepts unknown flags when not strict',
    fn: () => {
      return parseArgs({
        args: ['--unknown=value', '--flag'],
        strict: false,
      }).values;
    },
    expect: { unknown: 'value', flag: true },
  },
  {
    name: 'parseArgs aggregates multiple values',
    fn: () => {
      return parseArgs({
        options: { tag: { type: 'string', multiple: true } },
        args: ['--tag', 'alpha', '--tag', 'beta'],
      }).values;
    },
    expect: { tag: ['alpha', 'beta'] },
  },
  {
    name: 'parseArgs fills defaults',
    fn: () => {
      return parseArgs({
        options: { level: { type: 'string', default: 'info' } },
      }).values;
    },
    expect: { level: 'info' },
  },
  {
    name: 'MIMEType parses params and round-trips',
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
  {
    name: 'MIMEType params are mutable and lowercase',
    fn: () => {
      const mime = new MIMEType('application/json');
      mime.params.set('Charset', 'utf-8');
      mime.type = 'APPLICATION';
      mime.subtype = 'JSON';
      return mime.toString();
    },
    expect: 'application/json;charset=utf-8',
  },
  {
    name: 'MIMEType rejects invalid syntax',
    fn: () => {
      try {
        new MIMEType('?');
        return null;
      } catch (err: unknown) {
        if (err instanceof Error) {
          const errorWithCode = err as Error & { code?: string };
          return errorWithCode.code ?? null;
        }
        return null;
      }
    },
    expect: 'ERR_INVALID_MIME_SYNTAX',
  },
];

let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== util Compatibility Tests ===\n');

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
