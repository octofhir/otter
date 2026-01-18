/**
 * Chalk compatibility tests for Otter runtime
 * Tests terminal styling library
 */

import chalk from 'chalk';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect?: unknown;
  validator?: (result: unknown) => boolean;
}

const tests: TestCase[] = [
  // Basic colors
  { name: 'chalk.red()', fn: () => chalk.red('red text').includes('red text'), expect: true },
  { name: 'chalk.green()', fn: () => chalk.green('green text').includes('green text'), expect: true },
  { name: 'chalk.blue()', fn: () => chalk.blue('blue text').includes('blue text'), expect: true },
  { name: 'chalk.yellow()', fn: () => chalk.yellow('yellow text').includes('yellow text'), expect: true },
  { name: 'chalk.cyan()', fn: () => chalk.cyan('cyan text').includes('cyan text'), expect: true },
  { name: 'chalk.magenta()', fn: () => chalk.magenta('magenta text').includes('magenta text'), expect: true },
  { name: 'chalk.white()', fn: () => chalk.white('white text').includes('white text'), expect: true },
  { name: 'chalk.black()', fn: () => chalk.black('black text').includes('black text'), expect: true },
  { name: 'chalk.gray()', fn: () => chalk.gray('gray text').includes('gray text'), expect: true },

  // Bright colors
  { name: 'chalk.redBright()', fn: () => chalk.redBright('bright red').includes('bright red'), expect: true },
  { name: 'chalk.greenBright()', fn: () => chalk.greenBright('bright green').includes('bright green'), expect: true },
  { name: 'chalk.blueBright()', fn: () => chalk.blueBright('bright blue').includes('bright blue'), expect: true },

  // Background colors
  { name: 'chalk.bgRed()', fn: () => chalk.bgRed('bg red').includes('bg red'), expect: true },
  { name: 'chalk.bgGreen()', fn: () => chalk.bgGreen('bg green').includes('bg green'), expect: true },
  { name: 'chalk.bgBlue()', fn: () => chalk.bgBlue('bg blue').includes('bg blue'), expect: true },

  // Modifiers
  { name: 'chalk.bold()', fn: () => chalk.bold('bold text').includes('bold text'), expect: true },
  { name: 'chalk.dim()', fn: () => chalk.dim('dim text').includes('dim text'), expect: true },
  { name: 'chalk.italic()', fn: () => chalk.italic('italic text').includes('italic text'), expect: true },
  { name: 'chalk.underline()', fn: () => chalk.underline('underline text').includes('underline text'), expect: true },
  { name: 'chalk.strikethrough()', fn: () => chalk.strikethrough('strikethrough').includes('strikethrough'), expect: true },
  { name: 'chalk.inverse()', fn: () => chalk.inverse('inverse text').includes('inverse text'), expect: true },
  { name: 'chalk.hidden()', fn: () => chalk.hidden('hidden text').includes('hidden text'), expect: true },

  // Chaining
  { name: 'chalk.red.bold()', fn: () => chalk.red.bold('red bold').includes('red bold'), expect: true },
  { name: 'chalk.blue.underline()', fn: () => chalk.blue.underline('blue underline').includes('blue underline'), expect: true },
  { name: 'chalk.bgRed.white()', fn: () => chalk.bgRed.white('bg red white').includes('bg red white'), expect: true },
  { name: 'chalk.bold.red.bgWhite()', fn: () => chalk.bold.red.bgWhite('styled').includes('styled'), expect: true },

  // Nesting
  {
    name: 'nested styles',
    fn: () => {
      const result = chalk.red('red ' + chalk.blue('blue') + ' red');
      return result.includes('red') && result.includes('blue');
    },
    expect: true
  },

  // Template literals (tagged template)
  {
    name: 'chalk template literal',
    fn: () => {
      const result = chalk`{red red text}`;
      return result.includes('red text');
    },
    expect: true
  },

  // RGB colors
  {
    name: 'chalk.rgb()',
    fn: () => chalk.rgb(255, 136, 0)('orange').includes('orange'),
    expect: true
  },
  {
    name: 'chalk.bgRgb()',
    fn: () => chalk.bgRgb(255, 136, 0)('bg orange').includes('bg orange'),
    expect: true
  },

  // Hex colors
  {
    name: 'chalk.hex()',
    fn: () => chalk.hex('#FF8800')('hex orange').includes('hex orange'),
    expect: true
  },
  {
    name: 'chalk.bgHex()',
    fn: () => chalk.bgHex('#FF8800')('bg hex').includes('bg hex'),
    expect: true
  },

  // ANSI 256 colors
  {
    name: 'chalk.ansi256()',
    fn: () => chalk.ansi256(208)('ansi orange').includes('ansi orange'),
    expect: true
  },
  {
    name: 'chalk.bgAnsi256()',
    fn: () => chalk.bgAnsi256(208)('bg ansi').includes('bg ansi'),
    expect: true
  },

  // Reset
  {
    name: 'chalk.reset()',
    fn: () => chalk.reset('reset text').includes('reset text'),
    expect: true
  },

  // Visible (returns empty string if color not supported, otherwise returns text)
  {
    name: 'chalk.visible()',
    fn: () => {
      const result = chalk.visible('visible text');
      // visible() returns empty string when chalk.level is 0, otherwise returns the styled text
      return result === '' || result.includes('visible text');
    },
    expect: true
  },

  // Level detection
  {
    name: 'chalk.level is number',
    fn: () => typeof chalk.level === 'number',
    expect: true
  },

  // supportsColor (chalk v5+ may not have this, use level instead)
  {
    name: 'chalk.supportsColor or level exists',
    fn: () => chalk.supportsColor !== undefined || typeof chalk.level === 'number',
    expect: true
  },

  // Empty string handling
  {
    name: 'chalk.red empty string',
    fn: () => chalk.red('') === '',
    expect: true
  },

  // Multiple arguments (chalk v5 doesn't support multiple args, only template)
  {
    name: 'chalk single argument',
    fn: () => chalk.red('hello').includes('hello'),
    expect: true
  },

  // Chaining with hex
  {
    name: 'chalk.bold.hex()',
    fn: () => chalk.bold.hex('#FF0000')('bold red hex').includes('bold red hex'),
    expect: true
  },

  // Complex nesting
  {
    name: 'deeply nested styles',
    fn: () => {
      const result = chalk.red('a' + chalk.green('b' + chalk.blue('c') + 'b') + 'a');
      return result.includes('a') && result.includes('b') && result.includes('c');
    },
    expect: true
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Chalk Compatibility Tests ===\n');

for (const test of tests) {
  try {
    const result = test.fn();
    const resultStr = JSON.stringify(result);
    const expectStr = JSON.stringify(test.expect);

    if (test.validator ? test.validator(result) : resultStr === expectStr) {
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
