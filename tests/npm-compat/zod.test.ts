/**
 * Zod compatibility tests for Otter runtime
 * Tests schema validation library
 */

import { z } from 'zod';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Primitive types
  { name: 'z.string().parse', fn: () => z.string().parse('hello'), expect: 'hello' },
  { name: 'z.number().parse', fn: () => z.number().parse(42), expect: 42 },
  { name: 'z.boolean().parse', fn: () => z.boolean().parse(true), expect: true },
  { name: 'z.null().parse', fn: () => z.null().parse(null), expect: null },
  { name: 'z.undefined().parse', fn: () => z.undefined().parse(undefined), expect: undefined },
  { name: 'z.literal().parse', fn: () => z.literal('hello').parse('hello'), expect: 'hello' },

  // String validations
  { name: 'z.string().min()', fn: () => z.string().min(3).parse('hello'), expect: 'hello' },
  { name: 'z.string().max()', fn: () => z.string().max(10).parse('hello'), expect: 'hello' },
  { name: 'z.string().length()', fn: () => z.string().length(5).parse('hello'), expect: 'hello' },
  { name: 'z.string().email()', fn: () => z.string().email().parse('test@example.com'), expect: 'test@example.com' },
  { name: 'z.string().url()', fn: () => z.string().url().parse('https://example.com'), expect: 'https://example.com' },
  { name: 'z.string().uuid()', fn: () => z.string().uuid().parse('550e8400-e29b-41d4-a716-446655440000'), expect: '550e8400-e29b-41d4-a716-446655440000' },
  { name: 'z.string().regex()', fn: () => z.string().regex(/^[a-z]+$/).parse('hello'), expect: 'hello' },
  { name: 'z.string().startsWith()', fn: () => z.string().startsWith('he').parse('hello'), expect: 'hello' },
  { name: 'z.string().endsWith()', fn: () => z.string().endsWith('lo').parse('hello'), expect: 'hello' },
  { name: 'z.string().trim()', fn: () => z.string().trim().parse('  hello  '), expect: 'hello' },
  { name: 'z.string().toLowerCase()', fn: () => z.string().toLowerCase().parse('HELLO'), expect: 'hello' },
  { name: 'z.string().toUpperCase()', fn: () => z.string().toUpperCase().parse('hello'), expect: 'HELLO' },

  // Number validations
  { name: 'z.number().min()', fn: () => z.number().min(0).parse(5), expect: 5 },
  { name: 'z.number().max()', fn: () => z.number().max(10).parse(5), expect: 5 },
  { name: 'z.number().int()', fn: () => z.number().int().parse(42), expect: 42 },
  { name: 'z.number().positive()', fn: () => z.number().positive().parse(5), expect: 5 },
  { name: 'z.number().negative()', fn: () => z.number().negative().parse(-5), expect: -5 },
  { name: 'z.number().nonnegative()', fn: () => z.number().nonnegative().parse(0), expect: 0 },
  { name: 'z.number().nonpositive()', fn: () => z.number().nonpositive().parse(0), expect: 0 },
  { name: 'z.number().multipleOf()', fn: () => z.number().multipleOf(5).parse(15), expect: 15 },
  { name: 'z.number().finite()', fn: () => z.number().finite().parse(42), expect: 42 },
  { name: 'z.number().safe()', fn: () => z.number().safe().parse(42), expect: 42 },

  // BigInt
  { name: 'z.bigint().parse', fn: () => z.bigint().parse(BigInt(42)), expect: BigInt(42) },

  // Date
  { name: 'z.date().parse', fn: () => z.date().parse(new Date('2024-01-15')).toISOString().split('T')[0], expect: '2024-01-15' },

  // Arrays
  { name: 'z.array().parse', fn: () => z.array(z.number()).parse([1, 2, 3]), expect: [1, 2, 3] },
  { name: 'z.array().min()', fn: () => z.array(z.number()).min(2).parse([1, 2, 3]), expect: [1, 2, 3] },
  { name: 'z.array().max()', fn: () => z.array(z.number()).max(5).parse([1, 2, 3]), expect: [1, 2, 3] },
  { name: 'z.array().length()', fn: () => z.array(z.number()).length(3).parse([1, 2, 3]), expect: [1, 2, 3] },
  { name: 'z.array().nonempty()', fn: () => z.array(z.number()).nonempty().parse([1]), expect: [1] },

  // Objects
  {
    name: 'z.object().parse',
    fn: () => z.object({ name: z.string(), age: z.number() }).parse({ name: 'Alice', age: 30 }),
    expect: { name: 'Alice', age: 30 }
  },
  {
    name: 'z.object().strict()',
    fn: () => z.object({ name: z.string() }).strict().parse({ name: 'Alice' }),
    expect: { name: 'Alice' }
  },
  {
    name: 'z.object().passthrough()',
    fn: () => z.object({ name: z.string() }).passthrough().parse({ name: 'Alice', extra: 1 }),
    expect: { name: 'Alice', extra: 1 }
  },
  {
    name: 'z.object().partial()',
    fn: () => z.object({ name: z.string(), age: z.number() }).partial().parse({ name: 'Alice' }),
    expect: { name: 'Alice' }
  },
  {
    name: 'z.object().pick()',
    fn: () => z.object({ name: z.string(), age: z.number() }).pick({ name: true }).parse({ name: 'Alice' }),
    expect: { name: 'Alice' }
  },
  {
    name: 'z.object().omit()',
    fn: () => z.object({ name: z.string(), age: z.number() }).omit({ age: true }).parse({ name: 'Alice' }),
    expect: { name: 'Alice' }
  },
  {
    name: 'z.object().extend()',
    fn: () => z.object({ name: z.string() }).extend({ age: z.number() }).parse({ name: 'Alice', age: 30 }),
    expect: { name: 'Alice', age: 30 }
  },
  {
    name: 'z.object().merge()',
    fn: () => z.object({ a: z.number() }).merge(z.object({ b: z.string() })).parse({ a: 1, b: 'x' }),
    expect: { a: 1, b: 'x' }
  },

  // Optional and nullable
  { name: 'z.optional().parse (value)', fn: () => z.string().optional().parse('hello'), expect: 'hello' },
  { name: 'z.optional().parse (undefined)', fn: () => z.string().optional().parse(undefined), expect: undefined },
  { name: 'z.nullable().parse (value)', fn: () => z.string().nullable().parse('hello'), expect: 'hello' },
  { name: 'z.nullable().parse (null)', fn: () => z.string().nullable().parse(null), expect: null },
  { name: 'z.nullish().parse (null)', fn: () => z.string().nullish().parse(null), expect: null },
  { name: 'z.nullish().parse (undefined)', fn: () => z.string().nullish().parse(undefined), expect: undefined },

  // Unions and intersections
  { name: 'z.union().parse (first)', fn: () => z.union([z.string(), z.number()]).parse('hello'), expect: 'hello' },
  { name: 'z.union().parse (second)', fn: () => z.union([z.string(), z.number()]).parse(42), expect: 42 },
  {
    name: 'z.intersection().parse',
    fn: () => z.intersection(z.object({ a: z.number() }), z.object({ b: z.string() })).parse({ a: 1, b: 'x' }),
    expect: { a: 1, b: 'x' }
  },
  { name: 'z.discriminatedUnion()', fn: () => {
    const schema = z.discriminatedUnion('type', [
      z.object({ type: z.literal('a'), value: z.string() }),
      z.object({ type: z.literal('b'), value: z.number() }),
    ]);
    return schema.parse({ type: 'a', value: 'hello' });
  }, expect: { type: 'a', value: 'hello' } },

  // Tuples
  { name: 'z.tuple().parse', fn: () => z.tuple([z.string(), z.number()]).parse(['hello', 42]), expect: ['hello', 42] },
  { name: 'z.tuple().rest()', fn: () => z.tuple([z.string()]).rest(z.number()).parse(['hello', 1, 2, 3]), expect: ['hello', 1, 2, 3] },

  // Records and Maps
  { name: 'z.record().parse', fn: () => z.record(z.string(), z.number()).parse({ a: 1, b: 2 }), expect: { a: 1, b: 2 } },
  { name: 'z.map().parse', fn: () => {
    const m = z.map(z.string(), z.number()).parse(new Map([['a', 1], ['b', 2]]));
    return Array.from(m.entries());
  }, expect: [['a', 1], ['b', 2]] },

  // Sets
  { name: 'z.set().parse', fn: () => {
    const s = z.set(z.number()).parse(new Set([1, 2, 3]));
    return Array.from(s).sort();
  }, expect: [1, 2, 3] },

  // Enums
  { name: 'z.enum().parse', fn: () => z.enum(['apple', 'banana', 'cherry']).parse('banana'), expect: 'banana' },
  { name: 'z.nativeEnum().parse', fn: () => {
    enum Fruits { Apple = 'apple', Banana = 'banana' }
    return z.nativeEnum(Fruits).parse(Fruits.Banana);
  }, expect: 'banana' },

  // Coercion
  { name: 'z.coerce.string()', fn: () => z.coerce.string().parse(42), expect: '42' },
  { name: 'z.coerce.number()', fn: () => z.coerce.number().parse('42'), expect: 42 },
  { name: 'z.coerce.boolean()', fn: () => z.coerce.boolean().parse(1), expect: true },

  // Default and catch
  { name: 'z.default() (missing)', fn: () => z.string().default('default').parse(undefined), expect: 'default' },
  { name: 'z.default() (present)', fn: () => z.string().default('default').parse('value'), expect: 'value' },
  { name: 'z.catch() (valid)', fn: () => z.number().catch(0).parse(42), expect: 42 },
  { name: 'z.catch() (invalid)', fn: () => z.number().catch(0).parse('invalid'), expect: 0 },

  // Transform and refine
  { name: 'z.transform()', fn: () => z.string().transform(s => s.length).parse('hello'), expect: 5 },
  { name: 'z.refine() (pass)', fn: () => z.number().refine(n => n > 0).parse(5), expect: 5 },

  // safeParse
  { name: 'safeParse (success)', fn: () => z.string().safeParse('hello').success, expect: true },
  { name: 'safeParse (failure)', fn: () => z.string().safeParse(42).success, expect: false },

  // parseAsync
  { name: 'parseAsync', fn: async () => await z.string().parseAsync('hello'), expect: 'hello' },

  // Any and unknown
  { name: 'z.any().parse', fn: () => z.any().parse({ anything: true }), expect: { anything: true } },
  { name: 'z.unknown().parse', fn: () => z.unknown().parse({ anything: true }), expect: { anything: true } },

  // Never (should throw)
  { name: 'z.never() throws', fn: () => {
    try {
      z.never().parse('anything');
      return 'did not throw';
    } catch {
      return 'threw';
    }
  }, expect: 'threw' },

  // Void
  { name: 'z.void().parse', fn: () => z.void().parse(undefined), expect: undefined },

  // Promise
  { name: 'z.promise().parseAsync', fn: async () => {
    const p = await z.promise(z.string()).parseAsync(Promise.resolve('hello'));
    return await p;
  }, expect: 'hello' },

  // Lazy (recursive types)
  { name: 'z.lazy() (simple)', fn: () => {
    const schema: z.ZodType<string> = z.lazy(() => z.string());
    return schema.parse('hello');
  }, expect: 'hello' },

  // Preprocess
  { name: 'z.preprocess()', fn: () => {
    const schema = z.preprocess((val) => String(val), z.string());
    return schema.parse(42);
  }, expect: '42' },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Zod Compatibility Tests ===\n');

async function runTests() {
  for (const test of tests) {
    try {
      let result = test.fn();
      if (result instanceof Promise) {
        result = await result;
      }
      const resultStr = JSON.stringify(result, (_, v) => typeof v === 'bigint' ? v.toString() : v);
      const expectStr = JSON.stringify(test.expect, (_, v) => typeof v === 'bigint' ? v.toString() : v);

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
