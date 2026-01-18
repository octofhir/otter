/**
 * date-fns compatibility tests for Otter runtime
 * Tests core date manipulation functions
 */

import {
  format,
  parseISO,
  addDays,
  addMonths,
  addYears,
  subDays,
  differenceInDays,
  differenceInMonths,
  differenceInYears,
  isAfter,
  isBefore,
  isEqual,
  startOfDay,
  endOfDay,
  startOfMonth,
  endOfMonth,
  startOfYear,
  endOfYear,
  getDay,
  getMonth,
  getYear,
  setDay,
  setMonth,
  setYear,
  formatDistance,
  formatRelative,
  isValid,
  parse,
  compareAsc,
  compareDesc,
  max,
  min,
  isSameDay,
  isSameMonth,
  isSameYear,
  eachDayOfInterval,
  isWeekend,
  isWithinInterval,
} from 'date-fns';

interface TestCase {
  name: string;
  fn: () => unknown;
  expect: unknown;
  compare?: 'date' | 'string' | 'json';
}

// Fixed reference date for consistent tests
const refDate = new Date(2024, 0, 15, 12, 30, 0); // Jan 15, 2024 12:30:00
const refDate2 = new Date(2024, 5, 20, 8, 0, 0);  // Jun 20, 2024 08:00:00

const tests: TestCase[] = [
  // Formatting
  { name: 'format (yyyy-MM-dd)', fn: () => format(refDate, 'yyyy-MM-dd'), expect: '2024-01-15' },
  { name: 'format (HH:mm:ss)', fn: () => format(refDate, 'HH:mm:ss'), expect: '12:30:00' },
  { name: 'format (full)', fn: () => format(refDate, 'yyyy-MM-dd HH:mm:ss'), expect: '2024-01-15 12:30:00' },
  { name: 'format (EEEE)', fn: () => format(refDate, 'EEEE'), expect: 'Monday' },
  { name: 'format (MMMM)', fn: () => format(refDate, 'MMMM'), expect: 'January' },
  { name: 'format (do)', fn: () => format(refDate, 'do'), expect: '15th' },

  // Parsing
  { name: 'parseISO', fn: () => format(parseISO('2024-01-15'), 'yyyy-MM-dd'), expect: '2024-01-15' },
  { name: 'parse (yyyy-MM-dd)', fn: () => parse('2024-06-20', 'yyyy-MM-dd', new Date()).getMonth(), expect: 5 },
  { name: 'isValid (valid)', fn: () => isValid(refDate), expect: true },
  { name: 'isValid (invalid)', fn: () => isValid(new Date('invalid')), expect: false },

  // Add/Subtract
  { name: 'addDays', fn: () => format(addDays(refDate, 5), 'yyyy-MM-dd'), expect: '2024-01-20' },
  { name: 'addMonths', fn: () => format(addMonths(refDate, 2), 'yyyy-MM-dd'), expect: '2024-03-15' },
  { name: 'addYears', fn: () => format(addYears(refDate, 1), 'yyyy-MM-dd'), expect: '2025-01-15' },
  { name: 'subDays', fn: () => format(subDays(refDate, 10), 'yyyy-MM-dd'), expect: '2024-01-05' },

  // Differences (use startOfDay to get calendar day difference)
  { name: 'differenceInDays', fn: () => differenceInDays(startOfDay(refDate2), startOfDay(refDate)), expect: 157 },
  { name: 'differenceInMonths', fn: () => differenceInMonths(refDate2, refDate), expect: 5 },
  { name: 'differenceInYears', fn: () => differenceInYears(addYears(refDate, 2), refDate), expect: 2 },

  // Comparisons
  { name: 'isAfter (true)', fn: () => isAfter(refDate2, refDate), expect: true },
  { name: 'isAfter (false)', fn: () => isAfter(refDate, refDate2), expect: false },
  { name: 'isBefore (true)', fn: () => isBefore(refDate, refDate2), expect: true },
  { name: 'isBefore (false)', fn: () => isBefore(refDate2, refDate), expect: false },
  { name: 'isEqual (same)', fn: () => isEqual(refDate, new Date(refDate)), expect: true },
  { name: 'isEqual (different)', fn: () => isEqual(refDate, refDate2), expect: false },
  { name: 'compareAsc', fn: () => compareAsc(refDate, refDate2), expect: -1 },
  { name: 'compareDesc', fn: () => compareDesc(refDate, refDate2), expect: 1 },

  // Start/End of periods
  { name: 'startOfDay', fn: () => format(startOfDay(refDate), 'HH:mm:ss'), expect: '00:00:00' },
  { name: 'endOfDay', fn: () => format(endOfDay(refDate), 'HH:mm:ss'), expect: '23:59:59' },
  { name: 'startOfMonth', fn: () => format(startOfMonth(refDate), 'yyyy-MM-dd'), expect: '2024-01-01' },
  { name: 'endOfMonth', fn: () => format(endOfMonth(refDate), 'yyyy-MM-dd'), expect: '2024-01-31' },
  { name: 'startOfYear', fn: () => format(startOfYear(refDate), 'yyyy-MM-dd'), expect: '2024-01-01' },
  { name: 'endOfYear', fn: () => format(endOfYear(refDate), 'yyyy-MM-dd'), expect: '2024-12-31' },

  // Getters
  { name: 'getDay (Monday=1)', fn: () => getDay(refDate), expect: 1 },
  { name: 'getMonth (Jan=0)', fn: () => getMonth(refDate), expect: 0 },
  { name: 'getYear', fn: () => getYear(refDate), expect: 2024 },

  // Setters
  { name: 'setDay', fn: () => getDay(setDay(refDate, 3)), expect: 3 },
  { name: 'setMonth', fn: () => getMonth(setMonth(refDate, 5)), expect: 5 },
  { name: 'setYear', fn: () => getYear(setYear(refDate, 2025)), expect: 2025 },

  // Same checks
  { name: 'isSameDay (same)', fn: () => isSameDay(refDate, new Date(2024, 0, 15, 18, 0, 0)), expect: true },
  { name: 'isSameDay (different)', fn: () => isSameDay(refDate, refDate2), expect: false },
  { name: 'isSameMonth (same)', fn: () => isSameMonth(refDate, new Date(2024, 0, 20)), expect: true },
  { name: 'isSameMonth (different)', fn: () => isSameMonth(refDate, refDate2), expect: false },
  { name: 'isSameYear (same)', fn: () => isSameYear(refDate, refDate2), expect: true },
  { name: 'isSameYear (different)', fn: () => isSameYear(refDate, addYears(refDate, 1)), expect: false },

  // Min/Max
  { name: 'max', fn: () => format(max([refDate, refDate2]), 'yyyy-MM-dd'), expect: '2024-06-20' },
  { name: 'min', fn: () => format(min([refDate, refDate2]), 'yyyy-MM-dd'), expect: '2024-01-15' },

  // Weekend check
  { name: 'isWeekend (Monday)', fn: () => isWeekend(refDate), expect: false },
  { name: 'isWeekend (Saturday)', fn: () => isWeekend(new Date(2024, 0, 13)), expect: true },
  { name: 'isWeekend (Sunday)', fn: () => isWeekend(new Date(2024, 0, 14)), expect: true },

  // Interval
  {
    name: 'eachDayOfInterval',
    fn: () => eachDayOfInterval({ start: refDate, end: addDays(refDate, 2) }).length,
    expect: 3
  },
  {
    name: 'isWithinInterval (inside)',
    fn: () => isWithinInterval(new Date(2024, 0, 16), { start: refDate, end: addDays(refDate, 5) }),
    expect: true
  },
  {
    name: 'isWithinInterval (outside)',
    fn: () => isWithinInterval(new Date(2024, 0, 10), { start: refDate, end: addDays(refDate, 5) }),
    expect: false
  },

  // Relative formatting (just check it returns a string)
  { name: 'formatDistance', fn: () => typeof formatDistance(refDate, refDate2), expect: 'string' },
  { name: 'formatRelative', fn: () => typeof formatRelative(refDate, new Date()), expect: 'string' },
];

// Run tests
let passed = 0;
let failed = 0;
const failures: string[] = [];

console.log('=== date-fns Compatibility Tests ===\n');

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
