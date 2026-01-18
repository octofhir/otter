/**
 * UUID compatibility tests for Otter runtime
 * Tests UUID generation and validation
 */

import { v1, v3, v4, v5, validate, version, NIL, parse, stringify } from 'uuid';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
  validator?: (result: unknown) => boolean;
}

// UUID v4 regex pattern
const UUID_REGEX = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

// Namespace UUIDs for v3/v5
const DNS_NAMESPACE = '6ba7b810-9dad-11d1-80b4-00c04fd430c8';
const URL_NAMESPACE = '6ba7b811-9dad-11d1-80b4-00c04fd430c8';

const tests: TestCase[] = [
  // UUID v4 (random)
  {
    name: 'v4() generates valid UUID',
    fn: () => UUID_REGEX.test(v4()),
    expect: true
  },
  {
    name: 'v4() generates unique UUIDs',
    fn: () => {
      const uuids = new Set<string>();
      for (let i = 0; i < 100; i++) {
        uuids.add(v4());
      }
      return uuids.size;
    },
    expect: 100
  },
  {
    name: 'v4() format check',
    fn: () => {
      const uuid = v4();
      return uuid.length === 36 && uuid.split('-').length === 5;
    },
    expect: true
  },
  {
    name: 'version(v4()) returns 4',
    fn: () => version(v4()),
    expect: 4
  },

  // UUID v1 (timestamp-based)
  {
    name: 'v1() generates valid UUID',
    fn: () => UUID_REGEX.test(v1()),
    expect: true
  },
  {
    name: 'version(v1()) returns 1',
    fn: () => version(v1()),
    expect: 1
  },

  // UUID v3 (MD5 hash-based)
  {
    name: 'v3() with DNS namespace',
    fn: () => {
      const uuid = v3('example.com', DNS_NAMESPACE);
      return UUID_REGEX.test(uuid);
    },
    expect: true
  },
  {
    name: 'v3() is deterministic',
    fn: () => {
      const uuid1 = v3('test', DNS_NAMESPACE);
      const uuid2 = v3('test', DNS_NAMESPACE);
      return uuid1 === uuid2;
    },
    expect: true
  },
  {
    name: 'version(v3()) returns 3',
    fn: () => version(v3('test', DNS_NAMESPACE)),
    expect: 3
  },
  {
    name: 'v3() different inputs produce different UUIDs',
    fn: () => {
      const uuid1 = v3('test1', DNS_NAMESPACE);
      const uuid2 = v3('test2', DNS_NAMESPACE);
      return uuid1 !== uuid2;
    },
    expect: true
  },

  // UUID v5 (SHA-1 hash-based)
  {
    name: 'v5() with URL namespace',
    fn: () => {
      const uuid = v5('https://example.com', URL_NAMESPACE);
      return UUID_REGEX.test(uuid);
    },
    expect: true
  },
  {
    name: 'v5() is deterministic',
    fn: () => {
      const uuid1 = v5('test', URL_NAMESPACE);
      const uuid2 = v5('test', URL_NAMESPACE);
      return uuid1 === uuid2;
    },
    expect: true
  },
  {
    name: 'version(v5()) returns 5',
    fn: () => version(v5('test', URL_NAMESPACE)),
    expect: 5
  },

  // Validation
  {
    name: 'validate() valid v4 UUID',
    fn: () => validate(v4()),
    expect: true
  },
  {
    name: 'validate() valid v1 UUID',
    fn: () => validate(v1()),
    expect: true
  },
  {
    name: 'validate() NIL UUID',
    fn: () => validate(NIL),
    expect: true
  },
  {
    name: 'validate() invalid UUID (too short)',
    fn: () => validate('not-a-uuid'),
    expect: false
  },
  {
    name: 'validate() invalid UUID (wrong format)',
    fn: () => validate('xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx'),
    expect: false
  },
  {
    name: 'validate() invalid UUID (empty)',
    fn: () => validate(''),
    expect: false
  },
  {
    name: 'validate() with uppercase',
    fn: () => validate('A987FBC9-4BED-3078-AF07-9141BA07C9F3'),  // Valid RFC 4122 UUID with uppercase
    expect: true
  },

  // Version detection
  {
    name: 'version() NIL UUID returns 0',
    fn: () => version(NIL),
    expect: 0
  },
  {
    name: 'version() throws for invalid UUID',
    fn: () => {
      try {
        version('invalid');
        return 'did not throw';
      } catch {
        return 'threw';
      }
    },
    expect: 'threw'
  },

  // NIL UUID
  {
    name: 'NIL is all zeros',
    fn: () => NIL,
    expect: '00000000-0000-0000-0000-000000000000'
  },

  // parse and stringify
  {
    name: 'parse() returns Uint8Array',
    fn: () => {
      const bytes = parse(NIL);
      return bytes instanceof Uint8Array && bytes.length === 16;
    },
    expect: true
  },
  {
    name: 'stringify() from bytes',
    fn: () => {
      const bytes = parse(NIL);
      return stringify(bytes);
    },
    expect: '00000000-0000-0000-0000-000000000000'
  },
  {
    name: 'parse() and stringify() roundtrip',
    fn: () => {
      const original = v4();
      const bytes = parse(original);
      const restored = stringify(bytes);
      return original === restored;
    },
    expect: true
  },

  // Edge cases
  {
    name: 'v4() with options (random values)',
    fn: () => {
      const options = {
        random: [
          0x10, 0x91, 0x56, 0xbe, 0xc4, 0xfb, 0xc1, 0xea,
          0x71, 0xb4, 0xef, 0xe1, 0x67, 0x1c, 0x58, 0x36,
        ],
      };
      const uuid = v4(options);
      return UUID_REGEX.test(uuid);
    },
    expect: true
  },

  // Multiple v4 calls should all be valid
  {
    name: 'v4() stress test (1000 UUIDs)',
    fn: () => {
      for (let i = 0; i < 1000; i++) {
        const uuid = v4();
        if (!validate(uuid) || version(uuid) !== 4) {
          return false;
        }
      }
      return true;
    },
    expect: true
  },

  // v1 with options
  {
    name: 'v1() with clockseq option',
    fn: () => {
      const uuid = v1({ clockseq: 0x1234 });
      return validate(uuid) && version(uuid) === 1;
    },
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== UUID Compatibility Tests ===\n');

for (const test of tests) {
  try {
    const result = test.fn();
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
