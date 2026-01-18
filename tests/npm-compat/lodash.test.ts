/**
 * Lodash compatibility tests for Otter runtime
 * Tests core utility functions from lodash
 */

import * as _ from 'lodash-es';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
}

const tests: TestCase[] = [
  // Array utilities
  { name: '_.chunk', fn: () => _.chunk([1, 2, 3, 4, 5], 2), expect: [[1, 2], [3, 4], [5]] },
  { name: '_.compact', fn: () => _.compact([0, 1, false, 2, '', 3, null, undefined]), expect: [1, 2, 3] },
  { name: '_.concat', fn: () => _.concat([1], 2, [3], [[4]]), expect: [1, 2, 3, [4]] },
  { name: '_.difference', fn: () => _.difference([2, 1], [2, 3]), expect: [1] },
  { name: '_.drop', fn: () => _.drop([1, 2, 3], 2), expect: [3] },
  { name: '_.dropRight', fn: () => _.dropRight([1, 2, 3], 2), expect: [1] },
  { name: '_.fill', fn: () => _.fill([1, 2, 3], 'a'), expect: ['a', 'a', 'a'] },
  { name: '_.flatten', fn: () => _.flatten([1, [2, [3, [4]], 5]]), expect: [1, 2, [3, [4]], 5] },
  { name: '_.flattenDeep', fn: () => _.flattenDeep([1, [2, [3, [4]], 5]]), expect: [1, 2, 3, 4, 5] },
  { name: '_.head', fn: () => _.head([1, 2, 3]), expect: 1 },
  { name: '_.last', fn: () => _.last([1, 2, 3]), expect: 3 },
  { name: '_.take', fn: () => _.take([1, 2, 3], 2), expect: [1, 2] },
  { name: '_.uniq', fn: () => _.uniq([2, 1, 2, 3, 1]), expect: [2, 1, 3] },
  { name: '_.zip', fn: () => _.zip(['a', 'b'], [1, 2]), expect: [['a', 1], ['b', 2]] },

  // Object utilities
  { name: '_.get (simple)', fn: () => _.get({ a: 1 }, 'a'), expect: 1 },
  { name: '_.get (nested)', fn: () => _.get({ a: { b: { c: 3 } } }, 'a.b.c'), expect: 3 },
  { name: '_.get (array path)', fn: () => _.get({ a: [{ b: 1 }] }, 'a[0].b'), expect: 1 },
  { name: '_.get (default)', fn: () => _.get({ a: 1 }, 'b', 'default'), expect: 'default' },
  { name: '_.set', fn: () => { const o = {}; _.set(o, 'a.b.c', 1); return o; }, expect: { a: { b: { c: 1 } } } },
  { name: '_.has', fn: () => _.has({ a: { b: 1 } }, 'a.b'), expect: true },
  { name: '_.keys', fn: () => _.keys({ a: 1, b: 2 }), expect: ['a', 'b'] },
  { name: '_.values', fn: () => _.values({ a: 1, b: 2 }), expect: [1, 2] },
  { name: '_.entries', fn: () => _.toPairs({ a: 1, b: 2 }), expect: [['a', 1], ['b', 2]] },
  { name: '_.merge', fn: () => _.merge({ a: 1 }, { b: 2 }, { a: 3 }), expect: { a: 3, b: 2 } },
  { name: '_.mergeWith', fn: () => _.mergeWith({ a: [1] }, { a: [2] }, (o, s) => Array.isArray(o) ? o.concat(s) : undefined), expect: { a: [1, 2] } },
  { name: '_.pick', fn: () => _.pick({ a: 1, b: 2, c: 3 }, ['a', 'c']), expect: { a: 1, c: 3 } },
  { name: '_.omit', fn: () => _.omit({ a: 1, b: 2, c: 3 }, ['b']), expect: { a: 1, c: 3 } },
  { name: '_.defaults', fn: () => _.defaults({ a: 1 }, { a: 2, b: 2 }), expect: { a: 1, b: 2 } },
  { name: '_.cloneDeep', fn: () => { const o = { a: { b: 1 } }; const c = _.cloneDeep(o); c.a.b = 2; return o.a.b; }, expect: 1 },

  // Collection utilities
  { name: '_.map', fn: () => _.map([1, 2, 3], x => x * 2), expect: [2, 4, 6] },
  { name: '_.filter', fn: () => _.filter([1, 2, 3, 4], x => x % 2 === 0), expect: [2, 4] },
  { name: '_.find', fn: () => _.find([1, 2, 3, 4], x => x > 2), expect: 3 },
  { name: '_.findIndex', fn: () => _.findIndex([1, 2, 3, 4], x => x > 2), expect: 2 },
  { name: '_.reduce', fn: () => _.reduce([1, 2, 3], (sum, n) => sum + n, 0), expect: 6 },
  { name: '_.groupBy', fn: () => _.groupBy([6.1, 4.2, 6.3], Math.floor), expect: { '4': [4.2], '6': [6.1, 6.3] } },
  { name: '_.keyBy', fn: () => _.keyBy([{ id: 'a' }, { id: 'b' }], 'id'), expect: { a: { id: 'a' }, b: { id: 'b' } } },
  { name: '_.sortBy', fn: () => _.sortBy([{ n: 2 }, { n: 1 }, { n: 3 }], 'n'), expect: [{ n: 1 }, { n: 2 }, { n: 3 }] },
  { name: '_.orderBy', fn: () => _.orderBy([{ n: 2 }, { n: 1 }, { n: 3 }], ['n'], ['desc']), expect: [{ n: 3 }, { n: 2 }, { n: 1 }] },
  { name: '_.countBy', fn: () => _.countBy([6.1, 4.2, 6.3], Math.floor), expect: { '4': 1, '6': 2 } },
  { name: '_.partition', fn: () => _.partition([1, 2, 3, 4], x => x % 2), expect: [[1, 3], [2, 4]] },
  { name: '_.every', fn: () => _.every([2, 4, 6], x => x % 2 === 0), expect: true },
  { name: '_.some', fn: () => _.some([1, 2, 3], x => x > 2), expect: true },
  { name: '_.includes', fn: () => _.includes([1, 2, 3], 2), expect: true },

  // String utilities
  { name: '_.camelCase', fn: () => _.camelCase('Foo Bar'), expect: 'fooBar' },
  { name: '_.capitalize', fn: () => _.capitalize('FRED'), expect: 'Fred' },
  { name: '_.kebabCase', fn: () => _.kebabCase('Foo Bar'), expect: 'foo-bar' },
  { name: '_.snakeCase', fn: () => _.snakeCase('Foo Bar'), expect: 'foo_bar' },
  { name: '_.startCase', fn: () => _.startCase('--foo-bar--'), expect: 'Foo Bar' },
  { name: '_.upperFirst', fn: () => _.upperFirst('fred'), expect: 'Fred' },
  { name: '_.lowerCase', fn: () => _.lowerCase('--Foo-Bar--'), expect: 'foo bar' },
  { name: '_.trim', fn: () => _.trim('  abc  '), expect: 'abc' },
  { name: '_.pad', fn: () => _.pad('abc', 8), expect: '  abc   ' },
  { name: '_.repeat', fn: () => _.repeat('ab', 3), expect: 'ababab' },
  { name: '_.split', fn: () => _.split('a-b-c', '-'), expect: ['a', 'b', 'c'] },

  // Function utilities (basic)
  { name: '_.noop', fn: () => _.noop(), expect: undefined },
  { name: '_.identity', fn: () => _.identity(42), expect: 42 },
  { name: '_.constant', fn: () => _.constant(42)(), expect: 42 },
  { name: '_.times', fn: () => _.times(3, i => i * 2), expect: [0, 2, 4] },
  { name: '_.range', fn: () => _.range(4), expect: [0, 1, 2, 3] },
  { name: '_.range (start,end)', fn: () => _.range(1, 5), expect: [1, 2, 3, 4] },
  { name: '_.range (step)', fn: () => _.range(0, 10, 2), expect: [0, 2, 4, 6, 8] },

  // Type checks
  { name: '_.isArray', fn: () => _.isArray([1, 2, 3]), expect: true },
  { name: '_.isObject', fn: () => _.isObject({}), expect: true },
  { name: '_.isString', fn: () => _.isString('abc'), expect: true },
  { name: '_.isNumber', fn: () => _.isNumber(42), expect: true },
  { name: '_.isBoolean', fn: () => _.isBoolean(false), expect: true },
  { name: '_.isNull', fn: () => _.isNull(null), expect: true },
  { name: '_.isUndefined', fn: () => _.isUndefined(undefined), expect: true },
  { name: '_.isNil', fn: () => _.isNil(null), expect: true },
  { name: '_.isEmpty (empty)', fn: () => _.isEmpty([]), expect: true },
  { name: '_.isEmpty (not empty)', fn: () => _.isEmpty([1]), expect: false },
  { name: '_.isEqual', fn: () => _.isEqual({ a: 1 }, { a: 1 }), expect: true },
  { name: '_.isFunction', fn: () => _.isFunction(() => {}), expect: true },
  { name: '_.isPlainObject', fn: () => _.isPlainObject({}), expect: true },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== Lodash Compatibility Tests ===\n');

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

// Exit with error if any tests failed
if (failed > 0) {
  process.exit(1);
}
