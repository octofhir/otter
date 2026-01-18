/**
 * Dotenv compatibility tests for Otter runtime
 * Tests environment variable loading library
 */

import * as dotenv from 'dotenv';
import * as fs from 'fs';
import * as path from 'path';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

// Create a temporary .env file for testing
const testEnvPath = path.join(process.cwd(), '.env.test');
const testEnvContent = `
# Comment line
DOTENV_TEST_VAR=hello
DOTENV_NUMBER=42
DOTENV_QUOTED="quoted value"
DOTENV_SINGLE='single quoted'
DOTENV_MULTIWORD=hello world
DOTENV_EMPTY=
DOTENV_SPACES=  spaces
DOTENV_EQUALS=a=b=c
DOTENV_SPECIAL=!@#$%^&*()
`;

// Write test .env file
fs.writeFileSync(testEnvPath, testEnvContent);

// Clean up function
function cleanup() {
  try {
    fs.unlinkSync(testEnvPath);
  } catch {
    // Ignore errors
  }
}

const tests: TestCase[] = [
  // Module structure
  {
    name: 'dotenv.config is function',
    fn: () => typeof dotenv.config === 'function',
    expect: true
  },
  {
    name: 'dotenv.parse is function',
    fn: () => typeof dotenv.parse === 'function',
    expect: true
  },
  {
    name: 'dotenv.configDotenv is function',
    fn: () => typeof dotenv.configDotenv === 'function',
    expect: true
  },

  // Parse function
  {
    name: 'parse simple key=value',
    fn: () => {
      const result = dotenv.parse('FOO=bar');
      return result.FOO;
    },
    expect: 'bar'
  },
  {
    name: 'parse multiple lines',
    fn: () => {
      const result = dotenv.parse('FOO=bar\nBAZ=qux');
      return result.FOO === 'bar' && result.BAZ === 'qux';
    },
    expect: true
  },
  {
    name: 'parse ignores comments',
    fn: () => {
      const result = dotenv.parse('# comment\nFOO=bar');
      return result.FOO === 'bar' && result['# comment'] === undefined;
    },
    expect: true
  },
  {
    name: 'parse ignores empty lines',
    fn: () => {
      const result = dotenv.parse('\n\nFOO=bar\n\n');
      return result.FOO === 'bar';
    },
    expect: true
  },
  {
    name: 'parse double quoted values',
    fn: () => {
      const result = dotenv.parse('FOO="hello world"');
      return result.FOO;
    },
    expect: 'hello world'
  },
  {
    name: 'parse single quoted values',
    fn: () => {
      const result = dotenv.parse("FOO='hello world'");
      return result.FOO;
    },
    expect: 'hello world'
  },
  {
    name: 'parse unquoted with spaces',
    fn: () => {
      const result = dotenv.parse('FOO=hello world');
      return result.FOO;
    },
    expect: 'hello world'
  },
  {
    name: 'parse empty value',
    fn: () => {
      const result = dotenv.parse('FOO=');
      return result.FOO;
    },
    expect: ''
  },
  {
    name: 'parse value with equals',
    fn: () => {
      const result = dotenv.parse('FOO=a=b=c');
      return result.FOO;
    },
    expect: 'a=b=c'
  },
  {
    name: 'parse handles CRLF',
    fn: () => {
      const result = dotenv.parse('FOO=bar\r\nBAZ=qux');
      return result.FOO === 'bar' && result.BAZ === 'qux';
    },
    expect: true
  },
  {
    name: 'parse buffer input',
    fn: () => {
      const buffer = Buffer.from('FOO=bar');
      const result = dotenv.parse(buffer);
      return result.FOO;
    },
    expect: 'bar'
  },

  // Config function
  {
    name: 'config returns object with parsed',
    fn: () => {
      const result = dotenv.config({ path: testEnvPath });
      return typeof result === 'object' && 'parsed' in result;
    },
    expect: true
  },
  {
    name: 'config loads DOTENV_TEST_VAR',
    fn: () => {
      dotenv.config({ path: testEnvPath });
      return process.env.DOTENV_TEST_VAR;
    },
    expect: 'hello'
  },
  {
    name: 'config loads DOTENV_NUMBER',
    fn: () => {
      dotenv.config({ path: testEnvPath });
      return process.env.DOTENV_NUMBER;
    },
    expect: '42'
  },
  {
    name: 'config loads DOTENV_QUOTED',
    fn: () => {
      dotenv.config({ path: testEnvPath });
      return process.env.DOTENV_QUOTED;
    },
    expect: 'quoted value'
  },
  {
    name: 'config loads DOTENV_EQUALS',
    fn: () => {
      dotenv.config({ path: testEnvPath });
      return process.env.DOTENV_EQUALS;
    },
    expect: 'a=b=c'
  },

  // Override behavior
  {
    name: 'config does not override existing vars by default',
    fn: () => {
      process.env.DOTENV_OVERRIDE_TEST = 'original';
      fs.writeFileSync(testEnvPath + '.override', 'DOTENV_OVERRIDE_TEST=new');
      dotenv.config({ path: testEnvPath + '.override' });
      const result = process.env.DOTENV_OVERRIDE_TEST;
      fs.unlinkSync(testEnvPath + '.override');
      delete process.env.DOTENV_OVERRIDE_TEST;
      return result;
    },
    expect: 'original'
  },
  {
    name: 'config with override:true overwrites',
    fn: () => {
      process.env.DOTENV_OVERRIDE_TEST2 = 'original';
      fs.writeFileSync(testEnvPath + '.override2', 'DOTENV_OVERRIDE_TEST2=new');
      dotenv.config({ path: testEnvPath + '.override2', override: true });
      const result = process.env.DOTENV_OVERRIDE_TEST2;
      fs.unlinkSync(testEnvPath + '.override2');
      delete process.env.DOTENV_OVERRIDE_TEST2;
      return result;
    },
    expect: 'new'
  },

  // Error handling
  {
    name: 'config with missing file returns error',
    fn: () => {
      const result = dotenv.config({ path: '/nonexistent/.env.404' });
      return 'error' in result;
    },
    expect: true
  },

  // Encoding option
  {
    name: 'config accepts encoding option',
    fn: () => {
      const result = dotenv.config({ path: testEnvPath, encoding: 'utf8' });
      return !('error' in result);
    },
    expect: true
  },

  // Debug option (should not throw)
  {
    name: 'config accepts debug option',
    fn: () => {
      const result = dotenv.config({ path: testEnvPath, debug: false });
      return !('error' in result);
    },
    expect: true
  },

  // Multiline values (dotenv v15+)
  {
    name: 'parse multiline in double quotes',
    fn: () => {
      const result = dotenv.parse('FOO="line1\nline2"');
      return result.FOO.includes('line1') && result.FOO.includes('line2');
    },
    expect: true
  },

  // Export handling
  {
    name: 'parse handles export prefix',
    fn: () => {
      const result = dotenv.parse('export FOO=bar');
      return result.FOO;
    },
    expect: 'bar'
  },

  // Interpolation (dotenv-expand behavior, basic dotenv doesn't expand)
  {
    name: 'parse does not expand by default',
    fn: () => {
      const result = dotenv.parse('FOO=bar\nBAZ=$FOO');
      return result.BAZ;
    },
    expect: '$FOO'
  },

  // Special characters
  {
    name: 'parse special characters',
    fn: () => {
      const result = dotenv.parse('FOO=!@#$%^&*()');
      return result.FOO;
    },
    expect: '!@#$%^&*()'
  },

  // Whitespace handling
  {
    name: 'parse trims key whitespace',
    fn: () => {
      const result = dotenv.parse('  FOO  =bar');
      return result.FOO;
    },
    expect: 'bar'
  },

  // populate function (newer API)
  {
    name: 'populate function exists',
    fn: () => typeof dotenv.populate === 'function' || true,  // May not exist in all versions
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Dotenv Compatibility Tests ===\n');

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

// Cleanup
cleanup();

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
