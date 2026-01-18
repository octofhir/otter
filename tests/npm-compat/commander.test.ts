/**
 * Commander compatibility tests for Otter runtime
 * Tests CLI framework library
 */

import { Command, Option, Argument } from 'commander';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Basic command creation
  {
    name: 'new Command()',
    fn: () => {
      const program = new Command();
      return program instanceof Command;
    },
    expect: true
  },

  // Name and description
  {
    name: 'program.name()',
    fn: () => {
      const program = new Command();
      program.name('test-cli');
      return program.name();
    },
    expect: 'test-cli'
  },
  {
    name: 'program.description()',
    fn: () => {
      const program = new Command();
      program.description('A test CLI');
      return program.description();
    },
    expect: 'A test CLI'
  },
  {
    name: 'program.version()',
    fn: () => {
      const program = new Command();
      program.version('1.0.0');
      return program.version();
    },
    expect: '1.0.0'
  },

  // Options
  {
    name: 'option -f, --force',
    fn: () => {
      const program = new Command();
      program.option('-f, --force', 'force action');
      program.parse(['node', 'test', '--force']);
      return program.opts().force;
    },
    expect: true
  },
  {
    name: 'option with value -n <name>',
    fn: () => {
      const program = new Command();
      program.option('-n, --name <value>', 'your name');
      program.parse(['node', 'test', '-n', 'Alice']);
      return program.opts().name;
    },
    expect: 'Alice'
  },
  {
    name: 'option with default value',
    fn: () => {
      const program = new Command();
      program.option('-p, --port <number>', 'port number', '3000');
      program.parse(['node', 'test']);
      return program.opts().port;
    },
    expect: '3000'
  },
  {
    name: 'option override default',
    fn: () => {
      const program = new Command();
      program.option('-p, --port <number>', 'port number', '3000');
      program.parse(['node', 'test', '-p', '8080']);
      return program.opts().port;
    },
    expect: '8080'
  },
  {
    name: 'required option',
    fn: () => {
      const program = new Command();
      program.requiredOption('-c, --config <path>', 'config file');
      program.parse(['node', 'test', '-c', 'app.json']);
      return program.opts().config;
    },
    expect: 'app.json'
  },
  {
    name: 'boolean option negation --no-color',
    fn: () => {
      const program = new Command();
      program.option('--no-color', 'disable colors');
      program.parse(['node', 'test', '--no-color']);
      return program.opts().color;
    },
    expect: false
  },
  {
    name: 'variadic option -v <values...>',
    fn: () => {
      const program = new Command();
      program.option('-v, --values <items...>', 'multiple values');
      program.parse(['node', 'test', '-v', 'a', 'b', 'c']);
      return program.opts().values;
    },
    expect: ['a', 'b', 'c']
  },

  // Arguments
  {
    name: 'argument <name>',
    fn: () => {
      const program = new Command();
      let capturedArg = '';
      program
        .argument('<name>', 'the name')
        .action((name) => { capturedArg = name; });
      program.parse(['node', 'test', 'Alice']);
      return capturedArg;
    },
    expect: 'Alice'
  },
  {
    name: 'optional argument [name]',
    fn: () => {
      const program = new Command();
      let capturedArg = 'default';
      program
        .argument('[name]', 'the name')
        .action((name) => { if (name) capturedArg = name; });
      program.parse(['node', 'test']);
      return capturedArg;
    },
    expect: 'default'
  },
  {
    name: 'variadic argument <items...>',
    fn: () => {
      const program = new Command();
      let capturedArgs: string[] = [];
      program
        .argument('<items...>', 'the items')
        .action((items) => { capturedArgs = items; });
      program.parse(['node', 'test', 'a', 'b', 'c']);
      return capturedArgs;
    },
    expect: ['a', 'b', 'c']
  },

  // Subcommands
  {
    name: 'subcommand',
    fn: () => {
      const program = new Command();
      let subCalled = false;
      program
        .command('sub')
        .description('a subcommand')
        .action(() => { subCalled = true; });
      program.parse(['node', 'test', 'sub']);
      return subCalled;
    },
    expect: true
  },
  {
    name: 'subcommand with argument',
    fn: () => {
      const program = new Command();
      let capturedName = '';
      program
        .command('greet <name>')
        .action((name) => { capturedName = name; });
      program.parse(['node', 'test', 'greet', 'Bob']);
      return capturedName;
    },
    expect: 'Bob'
  },
  {
    name: 'subcommand with options',
    fn: () => {
      const program = new Command();
      let capturedOpts: Record<string, unknown> = {};
      program
        .command('build')
        .option('-o, --output <dir>', 'output directory')
        .action((opts) => { capturedOpts = opts; });
      program.parse(['node', 'test', 'build', '-o', 'dist']);
      return capturedOpts.output;
    },
    expect: 'dist'
  },

  // Action chaining
  {
    name: 'action receives options and command',
    fn: () => {
      const program = new Command();
      let hasOpts = false;
      let hasCmd = false;
      program
        .option('-v, --verbose')
        .action((opts, cmd) => {
          hasOpts = typeof opts === 'object';
          hasCmd = cmd instanceof Command;
        });
      program.parse(['node', 'test', '-v']);
      return hasOpts && hasCmd;
    },
    expect: true
  },

  // Option class
  {
    name: 'Option class',
    fn: () => {
      const opt = new Option('-d, --debug', 'enable debug');
      return opt.flags;
    },
    expect: '-d, --debug'
  },
  {
    name: 'Option with choices',
    fn: () => {
      const program = new Command();
      program.addOption(
        new Option('-s, --size <value>', 'size').choices(['small', 'medium', 'large'])
      );
      program.parse(['node', 'test', '-s', 'medium']);
      return program.opts().size;
    },
    expect: 'medium'
  },

  // Argument class
  {
    name: 'Argument class',
    fn: () => {
      const arg = new Argument('<name>', 'the name');
      return arg.name();
    },
    expect: 'name'
  },

  // Help
  {
    name: 'helpInformation() returns string',
    fn: () => {
      const program = new Command();
      program
        .name('myapp')
        .description('My application')
        .option('-v, --verbose', 'verbose output');
      const help = program.helpInformation();
      return typeof help === 'string' && help.includes('myapp');
    },
    expect: true
  },

  // Parsing modes
  {
    name: 'parseOptions returns known/unknown',
    fn: () => {
      const program = new Command();
      program.option('-a, --alpha');
      const result = program.parseOptions(['--alpha', '--beta']);
      return 'operands' in result || 'unknown' in result;
    },
    expect: true
  },

  // Allow excess arguments
  {
    name: 'allowExcessArguments()',
    fn: () => {
      const program = new Command();
      let args: string[] = [];
      program
        .allowExcessArguments()
        .argument('<first>')
        .action((first, opts, cmd) => {
          args = cmd.args;
        });
      program.parse(['node', 'test', 'one', 'two', 'three']);
      return args;
    },
    expect: ['one', 'two', 'three']
  },

  // Pass through options after --
  {
    name: 'passThroughOptions()',
    fn: () => {
      const program = new Command();
      let passedArgs: string[] = [];
      program
        .passThroughOptions()
        .argument('<cmd>')
        .action((cmd, opts, command) => {
          passedArgs = command.args.slice(1);
        });
      program.parse(['node', 'test', 'build', '--', '--flag']);
      return passedArgs.includes('--flag');
    },
    expect: true
  },

  // Environment variable for option
  {
    name: 'option from env variable',
    fn: () => {
      const program = new Command();
      program.addOption(
        new Option('-t, --token <value>').env('MY_TOKEN')
      );
      // Set env var before parsing
      process.env.MY_TOKEN = 'secret123';
      program.parse(['node', 'test']);
      const result = program.opts().token;
      delete process.env.MY_TOKEN;
      return result;
    },
    expect: 'secret123'
  },

  // Command aliases
  {
    name: 'command alias',
    fn: () => {
      const program = new Command();
      let called = false;
      program
        .command('install')
        .alias('i')
        .action(() => { called = true; });
      program.parse(['node', 'test', 'i']);
      return called;
    },
    expect: true
  },

  // Multiple option values with --opt val1 --opt val2
  {
    name: 'collecting option values',
    fn: () => {
      const program = new Command();
      const collect = (val: string, prev: string[]) => [...prev, val];
      program.option('-c, --collect <value>', 'collect values', collect, [] as string[]);
      program.parse(['node', 'test', '-c', 'a', '-c', 'b', '-c', 'c']);
      return program.opts().collect;
    },
    expect: ['a', 'b', 'c']
  },

  // Coercion function
  {
    name: 'option with coercion',
    fn: () => {
      const program = new Command();
      program.option('-p, --port <number>', 'port', (val) => parseInt(val, 10));
      program.parse(['node', 'test', '-p', '8080']);
      return program.opts().port;
    },
    expect: 8080
  },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Commander Compatibility Tests ===\n');

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
