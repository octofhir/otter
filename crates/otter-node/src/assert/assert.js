'use strict';
// `node:assert` — JS surface. Deep-equality and value rendering come from `util`
// (injected); CallTracker and the Myers diff live in injected internal modules
// (internal/assert/calltracker, internal/assert/myers_diff). A real
// `AssertionError` class carries the correct name/code/actual/expected/operator
// so matcher checks observe it; the `Assert` class is the constructible form.

const util = require('util');
const { isDeepStrictEqual, isDeepEqual, inspect, getCallSites } = util;
const makeCallTracker = require('internal/assert/calltracker');
const kAssertCache = Symbol.for('otter.node.assert.exports');

if (typeof globalThis !== 'undefined' && globalThis[kAssertCache]) {
  module.exports = globalThis[kAssertCache];
  return;
}

function inspectValue(v) {
  return inspect(v, { depth: null, breakLength: Infinity, compact: 3 });
}

// Strict operators render a line-by-line +/- diff; loose operators render the
// two values around a "should (not) loosely (deep-)equal" separator.
const kDiffOperators = new Set([
  'deepStrictEqual', 'notDeepStrictEqual', 'partialDeepStrictEqual',
  'strictEqual', 'notStrictEqual',
]);
const kLooseOperators = {
  deepEqual: 'should loosely deep-equal',
  notDeepEqual: 'should not loosely deep-equal',
  equal: 'should loosely equal',
  notEqual: 'should not loosely equal',
};
const kLooseHeaders = {
  deepEqual: 'Expected values to be loosely deep-equal:',
  notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
  equal: 'Expected values to be loosely equal:',
  notEqual: 'Expected "actual" not to be loosely equal to:',
};
function looseDiffMessage(actual, expected, operator) {
  const opts = {
    compact: false, depth: 1000, customInspect: false,
    maxArrayLength: Infinity, breakLength: Infinity, sorted: true, getters: true,
  };
  return `${kLooseHeaders[operator]}\n\n${inspect(actual, opts)}\n\n` +
    `${kLooseOperators[operator]}\n\n${inspect(expected, opts)}`;
}

// Per-operator header for a generated diff message (replaced by a custom
// message when one is supplied).
const kDiffHeaders = {
  strictEqual: 'Expected values to be strictly equal:',
  notStrictEqual: 'Expected "actual" to be strictly unequal to:',
  deepStrictEqual: 'Expected values to be strictly deep-equal:',
  notDeepStrictEqual: 'Expected "actual" not to be strictly deep-equal to:',
  deepEqual: 'Expected values to be loosely deep-equal:',
  notDeepEqual: 'Expected "actual" not to be loosely deep-equal to:',
  equal: 'Expected values to be loosely equal:',
  partialDeepStrictEqual: 'Expected values to be partially and strictly deep-equal:',
};

// Two inspected lines are "the same" for diff purposes if they are equal or
// differ only by a trailing comma (the last element of a block loses its comma
// when it has no following sibling). Node keeps the comma'd form so an added
// sibling shows as a single inserted line, not a remove+add of the element.
function lineEq(a, b) {
  return a === b ||
    (a.endsWith(',') && a.slice(0, -1) === b) ||
    (b.endsWith(',') && b.slice(0, -1) === a);
}
function commonLine(a, b) {
  return a.endsWith(',') ? a : b;
}

// §createErrDiff — line-by-line LCS of inspect(actual)/inspect(expected) (both
// compact:false so each entry is its own line), prefixed `  ` (common) / `+ `
// (actual) / `- ` (expected) under a "+ actual - expected" legend. Comma-aware
// (see lineEq), mirroring Node's lib/internal/assert/assertion_error.js.
function diffLines(a, e) {
  const n = a.length;
  const m = e.length;
  const dp = [];
  for (let i = 0; i <= n; i++) dp.push(new Array(m + 1).fill(0));
  for (let i = n - 1; i >= 0; i--) {
    for (let j = m - 1; j >= 0; j--) {
      dp[i][j] = lineEq(a[i], e[j])
        ? dp[i + 1][j + 1] + 1
        : Math.max(dp[i + 1][j], dp[i][j + 1]);
    }
  }
  const out = [];
  let i = 0;
  let j = 0;
  while (i < n && j < m) {
    if (lineEq(a[i], e[j])) { out.push(`  ${commonLine(a[i], e[j])}`); i++; j++; } else if (dp[i + 1][j] >= dp[i][j + 1]) { out.push(`+ ${a[i]}`); i++; } else { out.push(`- ${e[j]}`); j++; }
  }
  while (i < n) out.push(`+ ${a[i++]}`);
  while (j < m) out.push(`- ${e[j++]}`);
  return out;
}

function createErrDiff(actual, expected, prefix) {
  const opts = {
    compact: false, breakLength: Infinity, depth: 1000,
    customInspect: false, sorted: true,
  };
  const actualLines = inspect(actual, opts).split('\n');
  const expectedLines = inspect(expected, opts).split('\n');
  const compacted = compactDiffLines(diffLines(actualLines, expectedLines));
  const skipped = compacted.skipped ? '... Skipped lines\n' : '';
  return `${prefix}\n+ actual - expected\n${skipped}\n${compacted.lines.join('\n')}\n`;
}

function compactDiffLines(lines) {
  if (lines.length <= 9) return { lines, skipped: false };
  const firstDiff = lines.findIndex((line) => line.startsWith('+ ') || line.startsWith('- '));
  if (firstDiff <= 7) return { lines, skipped: false };
  const prefix = lines.slice(0, 5);
  const suffixStart = Math.max(5, firstDiff - 1);
  return {
    lines: prefix.concat(['...'], lines.slice(suffixStart)),
    skipped: true,
  };
}

function hasNoEnumerableKeys(value) {
  return Object.keys(value).length === 0 && Object.getOwnPropertySymbols(value).every((sym) => {
    const desc = Object.getOwnPropertyDescriptor(value, sym);
    return !desc || !desc.enumerable;
  });
}

function isSimpleStrictEqualOperand(value, rendered) {
  if (rendered.includes('\n') || rendered.length > 50) return false;
  if (value === null || value === undefined) return true;
  const type = typeof value;
  if (type !== 'object') return type !== 'function';
  if (value instanceof RegExp) return true;
  if (Array.isArray(value)) return value.length === 0 && hasNoEnumerableKeys(value);
  return Object.getPrototypeOf(value) === Object.prototype && hasNoEnumerableKeys(value);
}

function canUseSimpleStrictEqualMessage(actual, expected, actualRendered, expectedRendered) {
  return isSimpleStrictEqualOperand(actual, actualRendered) &&
    isSimpleStrictEqualOperand(expected, expectedRendered);
}

function strictEqualMessage(actual, expected, prefix) {
  if (actual instanceof Error && expected instanceof Error) {
    return createErrDiff(
      actual,
      expected,
      'Expected "actual" to be reference-equal to "expected":'
    );
  }
  if (actual !== null && expected !== null &&
      (typeof actual === 'object' || typeof actual === 'function') &&
      (typeof expected === 'object' || typeof expected === 'function')) {
    if (Object.getPrototypeOf(actual) === Object.getPrototypeOf(expected) &&
        isDeepStrictEqual(actual, expected)) {
      return `Values have same structure but are not reference-equal:\n\n${inspectValue(actual)}\n`;
    }
    return createErrDiff(
      actual,
      expected,
      'Expected "actual" to be reference-equal to "expected":'
    );
  }
  const actualRendered = inspectValue(actual);
  const expectedRendered = inspectValue(expected);
  if (canUseSimpleStrictEqualMessage(actual, expected, actualRendered, expectedRendered)) {
    return `${prefix}\n\n${actualRendered} !== ${expectedRendered}\n`;
  }
  return createErrDiff(actual, expected, prefix);
}

function notStrictEqualMessage(actual) {
  const rendered = inspectValue(actual);
  if (!rendered.includes('\n') && rendered.length <= 50) {
    return `${kDiffHeaders.notStrictEqual} ${rendered}`;
  }
  return `${kDiffHeaders.notStrictEqual}\n\n${rendered}`;
}

function sanitizeSource(source) {
  return String(source).replace(/[\x00-\x1f]/g, (ch) => {
    const code = ch.charCodeAt(0);
    if (ch === '\t') return '\t';
    return `\\u${code.toString(16).padStart(4, '0')}`;
  });
}

function sliceCallExpression(line, start) {
  let depth = 0;
  let quote = '';
  let sawCallParen = false;
  for (let i = start; i < line.length; i++) {
    const ch = line[i];
    if (quote) {
      if (ch === '\\') {
        i++;
      } else if (ch === quote) {
        quote = '';
      }
      continue;
    }
    if (ch === '"' || ch === "'" || ch === '`') {
      quote = ch;
    } else if (ch === '(' || ch === '[' || ch === '{') {
      if (ch === '(') sawCallParen = true;
      depth++;
    } else if (ch === ')' || ch === ']' || ch === '}') {
      depth--;
      if (depth === 0 && sawCallParen) return line.slice(start, i + 1).trim();
    } else if (ch === ';' && depth <= 0) {
      return line.slice(start, i).trim();
    }
  }
  return line.slice(start).trim();
}

function findLastCallStart(line, patterns) {
  for (const [pattern, offset] of patterns) {
    const idx = line.lastIndexOf(pattern);
    if (idx >= 0) return idx + (offset || 0);
  }
  return -1;
}

function assertionSourceExpression() {
  if (typeof getCallSites !== 'function') return undefined;
  let sites;
  try {
    sites = getCallSites(8);
  } catch {
    return undefined;
  }
  const primaryPatterns = [
    ['assert[', 0],
    ['assert.ok', 0],
    ['strict.ok', 0],
    ['assert(', 0],
    ['.ok(', 1],
  ];
  const fallbackPatterns = [
    ['fn(', 0],
  ];
  for (const patterns of [primaryPatterns, fallbackPatterns]) {
    for (const candidate of sites) {
      if (!candidate ||
          candidate.scriptName === 'node:assert' || candidate.scriptName === 'assert' ||
          candidate.scriptName === 'node:test' || candidate.scriptName === 'test') {
        continue;
      }
      if (patterns === primaryPatterns && candidate.sourceLine) {
        const line = sanitizeSource(candidate.sourceLine);
        const start = findLastCallStart(line, fallbackPatterns);
        if (start >= 0) return sliceCallExpression(line, start);
      }
      const forward = Array.isArray(candidate.sourceLinesAfter) ? candidate.sourceLinesAfter : [];
      for (const source of [candidate.sourceLine, candidate.sourceLineBefore, candidate.sourceLineAfter, ...forward]) {
        if (!source) continue;
        const line = sanitizeSource(source);
        const start = findLastCallStart(line, patterns);
        if (start >= 0) return sliceCallExpression(line, start);
      }
    }
  }
  return undefined;
}

function generatedOkMessage() {
  const expression = assertionSourceExpression();
  if (!expression) return undefined;
  return `The expression evaluated to a falsy value:\n\n  ${expression}\n`;
}

class AssertionError extends Error {
  constructor(options) {
    if (arguments.length === 0) options = {};
    if (options === null || typeof options !== 'object') {
      const e = new TypeError(
        `The "options" argument must be of type object.${invalidArgTypeSuffix(options)}`
      );
      e.code = 'ERR_INVALID_ARG_TYPE';
      throw e;
    }
    const { message, actual, expected, operator, stackStartFn } = options;
    let msg = message;
    let generatedMessage = false;
    const wantsDiff = kDiffOperators.has(operator) &&
      !(typeof operator === 'string' && operator.startsWith('not'));
    const wantsLoose = Object.prototype.hasOwnProperty.call(kLooseOperators, operator);
    if (msg === undefined) {
      generatedMessage = true;
      if (operator === 'fail') {
        msg = 'Failed';
      } else if (operator === 'strictEqual') {
        msg = strictEqualMessage(actual, expected, kDiffHeaders.strictEqual);
      } else if (wantsDiff) {
        msg = createErrDiff(actual, expected, kDiffHeaders[operator] || '');
      } else if (wantsLoose) {
        msg = looseDiffMessage(actual, expected, operator);
      } else if (operator === 'notStrictEqual') {
        msg = notStrictEqualMessage(actual);
      } else if (operator === 'notDeepStrictEqual') {
        msg = `${kDiffHeaders.notDeepStrictEqual}\n\n${inspect(actual, {
          compact: false, breakLength: Infinity, depth: 1000,
          customInspect: false, sorted: true,
        })}`;
      } else {
        const op = operator || 'deepStrictEqual';
        msg = `${inspectValue(actual)} ${op} ${inspectValue(expected)}`;
      }
    } else if (operator === 'strictEqual') {
      msg = strictEqualMessage(actual, expected, message);
    } else if (wantsDiff) {
      // An explicit message replaces the header but keeps the diff.
      msg = createErrDiff(actual, expected, message);
    } else {
      msg = String(msg);
    }
    if (Object.prototype.hasOwnProperty.call(options, 'generatedMessage')) {
      generatedMessage = Boolean(options.generatedMessage);
    }
    super(msg);
    this.name = 'AssertionError';
    this.code = 'ERR_ASSERTION';
    this.actual = actual;
    this.expected = expected;
    this.operator = operator;
    this.generatedMessage = generatedMessage;
    if (Error.captureStackTrace && stackStartFn) {
      Error.captureStackTrace(this, stackStartFn);
    }
  }

  get [Symbol.toStringTag]() { return 'Error'; }
}

function innerFail(obj) {
  if (obj && obj.message instanceof Error) throw obj.message;
  throw new AssertionError(obj);
}

// §validateArgumentCount — the equality helpers require both `actual` and
// `expected`; a single argument is ERR_MISSING_ARGS, mirroring Node.
function requireTwoArgs(len) {
  if (len < 2) {
    const e = new TypeError(
      'The "actual" and "expected" arguments must be specified'
    );
    e.code = 'ERR_MISSING_ARGS';
    throw e;
  }
}

function regexpArgError(value) {
  const t = typeof value;
  const received =
    value === null ? 'null'
      : value === undefined ? 'undefined'
        : t === 'string' ? `type string ('${value}')`
          : t === 'object' ? `an instance of ${value.constructor ? value.constructor.name : 'Object'}`
            : `type ${t} (${String(value)})`;
  const e = new TypeError(
    `The "regexp" argument must be an instance of RegExp. Received ${received}`
  );
  e.code = 'ERR_INVALID_ARG_TYPE';
  return e;
}

function invalidArgTypeSuffix(input) {
  if (input === null || input === undefined) return ` Received ${input}`;
  if (typeof input === 'function') return ` Received function ${input.name}`;
  if (typeof input === 'object') {
    if (input.constructor && input.constructor.name) {
      return ` Received an instance of ${input.constructor.name}`;
    }
    return ' Received an instance of Object';
  }
  if (typeof input === 'string') return ` Received type string ('${input}')`;
  return ` Received type ${typeof input} (${String(input)})`;
}

function ok(...args) {
  const value = args[0];
  if (!value) {
    if (args[1] instanceof Error) throw args[1];
    if (args.length === 0) {
      innerFail({
        actual: value,
        expected: true,
        message: 'No value argument passed to `assert.ok()`',
        generatedMessage: true,
        operator: '==',
        stackStartFn: ok,
      });
    }
    innerFail({
      actual: value,
      expected: true,
      message: args.length > 1 && args[1] !== undefined ? args[1] : generatedOkMessage(),
      generatedMessage: args.length <= 1 || args[1] === undefined,
      operator: '==',
      stackStartFn: ok,
    });
  }
}

function assert(...args) {
  ok(...args);
}

function strictEqual(actual, expected, message) {
  if (!Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'strictEqual', stackStartFn: strictEqual });
  }
}
function notStrictEqual(actual, expected, message) {
  if (Object.is(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'notStrictEqual', stackStartFn: notStrictEqual });
  }
}
function equal(actual, expected, message) {
  // eslint-disable-next-line eqeqeq
  if (actual != expected && !(Number.isNaN(actual) && Number.isNaN(expected))) {
    innerFail({ actual, expected, message, operator: '==', stackStartFn: equal });
  }
}
function notEqual(actual, expected, message) {
  // eslint-disable-next-line eqeqeq
  if (actual == expected || (Number.isNaN(actual) && Number.isNaN(expected))) {
    innerFail({ actual, expected, message, operator: '!=', stackStartFn: notEqual });
  }
}

function deepStrictEqual(actual, expected, message) {
  if (!isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'deepStrictEqual', stackStartFn: deepStrictEqual });
  }
}
function notDeepStrictEqual(actual, expected, message) {
  if (isDeepStrictEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'notDeepStrictEqual', stackStartFn: notDeepStrictEqual });
  }
}

function deepStrictEqualSkipPrototype(actual, expected, message) {
  if (!isDeepStrictEqual(actual, expected, true)) {
    innerFail({ actual, expected, message, operator: 'deepStrictEqual', stackStartFn: deepStrictEqualSkipPrototype });
  }
}
function notDeepStrictEqualSkipPrototype(actual, expected, message) {
  if (isDeepStrictEqual(actual, expected, true)) {
    innerFail({ actual, expected, message, operator: 'notDeepStrictEqual', stackStartFn: notDeepStrictEqualSkipPrototype });
  }
}

// Loose deep equality reuses util's comparison engine (Date/RegExp expandos,
// Map/Set structural keys, boxed primitives, Error name/message/cause) so
// assert.deepEqual and util.isDeepStrictEqual stay consistent.
function deepEqual(actual, expected, message) {
  requireTwoArgs(arguments.length);
  if (!isDeepEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'deepEqual', stackStartFn: deepEqual });
  }
}
function notDeepEqual(actual, expected, message) {
  requireTwoArgs(arguments.length);
  if (isDeepEqual(actual, expected)) {
    innerFail({ actual, expected, message, operator: 'notDeepEqual', stackStartFn: notDeepEqual });
  }
}

function partialDeepStrictEqual(actual, expected, message) {
  if (!partialMatch(actual, expected, new Map())) {
    innerFail({ actual, expected, message, operator: 'partialDeepStrictEqual', stackStartFn: partialDeepStrictEqual });
  }
}
// Maximum-bipartite-matching subset check: every expected item must map to a
// DISTINCT actual item it matches. A greedy first-match is unsound — an empty
// `{}` expected matches both `{}` and `[]`, so consuming the wrong one can
// starve a stricter expected item. Kuhn's augmenting-path algorithm finds a
// complete matching whenever one exists. `canMatch(ei, aj)` is the precomputed
// adjacency: expected item `ei` is compatible with actual item `aj`.
function subsetMatchExists(expectedCount, actualCount, canMatch) {
  if (expectedCount > actualCount) return false;
  const actualToExpected = new Array(actualCount).fill(-1);
  const assign = (ei, visited) => {
    for (let aj = 0; aj < actualCount; aj++) {
      if (visited[aj] || !canMatch(ei, aj)) continue;
      visited[aj] = true;
      if (actualToExpected[aj] === -1 || assign(actualToExpected[aj], visited)) {
        actualToExpected[aj] = ei;
        return true;
      }
    }
    return false;
  };
  for (let ei = 0; ei < expectedCount; ei++) {
    if (!assign(ei, new Array(actualCount).fill(false))) return false;
  }
  return true;
}

function partialMatch(actual, expected, seen) {
  if (Object.is(actual, expected)) return true;
  if (typeof expected !== 'object' || expected === null) return Object.is(actual, expected);
  if (typeof actual !== 'object' || actual === null) return false;
  // Cycle guard keyed on the (expected → actual) PAIR with recursion-stack
  // semantics (popped on exit, below). When `expected` is re-encountered on
  // the current path it must close against the SAME `actual` — a circular
  // expected matched against an unrelated actual is NOT a back-edge and must
  // be rejected, not vacuously accepted.
  if (seen.has(expected)) return seen.get(expected) === actual;
  seen.set(expected, actual);
  const result = partialMatchBody(actual, expected, seen);
  seen.delete(expected);
  return result;
}

function partialMatchBody(actual, expected, seen) {
  // Each candidate comparison gets its own copy of the cycle context so a
  // failed match attempt cannot pollute the `seen` map for sibling
  // candidates explored by the bipartite matcher.
  const childMatch = (a, e) => partialMatch(a, e, new Map(seen));
  const indexKeysMatchedByContainer = Array.isArray(expected) ||
    (ArrayBuffer.isView(expected) && !(expected instanceof DataView));
  if (Array.isArray(expected)) {
    if (!Array.isArray(actual)) return false;
    // Each expected element must match a DISTINCT actual element (a
    // consumed subset), not merely some element — so duplicates in
    // expected need duplicates in actual.
    const adj = expected.map((e) => actual.map((a) => childMatch(a, e)));
    if (!subsetMatchExists(expected.length, actual.length, (ei, aj) => adj[ei][aj])) {
      return false;
    }
  } else if (ArrayBuffer.isView(expected) && !(expected instanceof DataView)) {
    if (!ArrayBuffer.isView(actual) || actual instanceof DataView) return false;
    if (Object.getPrototypeOf(actual) !== Object.getPrototypeOf(expected)) return false;
    const expectedValues = Array.from(expected);
    const actualValues = Array.from(actual);
    const adj = expectedValues.map((e) => actualValues.map((a) => Object.is(a, e)));
    if (!subsetMatchExists(expectedValues.length, actualValues.length, (ei, aj) => adj[ei][aj])) {
      return false;
    }
  } else if (expected instanceof Set) {
    // A Set/Map expected value is a containment check: every expected
    // member must match a DISTINCT actual member (so duplicates in the
    // expected collection demand duplicates in the actual one). Own
    // enumerable properties are then compared by the key loop below.
    if (!(actual instanceof Set)) return false;
    const actualValues = [...actual];
    const expectedValues = [...expected];
    const adj = expectedValues.map((e) => actualValues.map((a) => childMatch(a, e)));
    if (!subsetMatchExists(expectedValues.length, actualValues.length, (ei, aj) => adj[ei][aj])) {
      return false;
    }
  } else if (expected instanceof Map) {
    if (!(actual instanceof Map)) return false;
    const actualEntries = [...actual];
    const expectedEntries = [...expected];
    const adj = expectedEntries.map(([ek, ev]) =>
      actualEntries.map(([ak, av]) => childMatch(ak, ek) && childMatch(av, ev)));
    if (!subsetMatchExists(expectedEntries.length, actualEntries.length, (ei, aj) => adj[ei][aj])) {
      return false;
    }
  } else if (expected instanceof ArrayBuffer || expected instanceof SharedArrayBuffer) {
    if (Object.getPrototypeOf(actual) !== Object.getPrototypeOf(expected)) return false;
    if (actual.byteLength < expected.byteLength) return false;
    const actualBytes = new Uint8Array(actual);
    const expectedBytes = new Uint8Array(expected);
    for (let i = 0; i < expectedBytes.length; i++) {
      if (actualBytes[i] !== expectedBytes[i]) return false;
    }
  }
  for (const k of partialEnumerableKeys(expected, indexKeysMatchedByContainer)) {
    if (!Reflect.has(actual, k)) return false;
    if (!partialMatch(actual[k], expected[k], seen)) return false;
  }
  return true;
}

function partialEnumerableKeys(obj, skipIndexKeys) {
  const keys = [];
  for (const key of Object.keys(obj)) {
    if (!Object.prototype.propertyIsEnumerable.call(obj, key)) continue;
    if (skipIndexKeys && /^(0|[1-9]\d*)$/.test(key)) continue;
    keys.push(key);
  }
  let symbols = [];
  try {
    symbols = Object.getOwnPropertySymbols(obj);
  } catch {
    return keys;
  }
  for (const sym of symbols) {
    const desc = Object.getOwnPropertyDescriptor(obj, sym);
    if (desc && desc.enumerable) keys.push(sym);
  }
  return keys;
}

function ifError(value) {
  if (value !== null && value !== undefined) {
    let message = 'ifError got unwanted exception: ';
    if (typeof value === 'object' && typeof value.message === 'string') {
      message += value.message.length === 0 && value.constructor ? value.constructor.name : value.message;
    } else {
      message += inspectValue(value);
    }
    const err = new AssertionError({
      actual: value, expected: null, operator: 'ifError', message, stackStartFn: ifError,
    });
    err.generatedMessage = false;
    throw err;
  }
}

// §assert.fail — the multi-argument form is deprecated; with exactly two
// arguments the operator defaults to "!=" and the message is generated.
function fail(actual, expected, message, operator, stackStartFn) {
  const argsLen = arguments.length;
  if (argsLen === 1) {
    // fail(message) — the lone argument is the message.
    message = actual;
    actual = undefined;
  } else if (argsLen === 2) {
    // Deprecated two-argument form: operator defaults to "!=" and the
    // message is generated by AssertionError (generatedMessage stays true).
    operator = '!=';
  }
  if (message instanceof Error) throw message;
  innerFail({
    actual,
    expected,
    message,
    operator: operator || 'fail',
    stackStartFn: stackStartFn || fail,
  });
}

// ---- throws / rejects ----
function isErrorConstructor(fn) {
  if (typeof fn !== 'function') return false;
  let proto = fn.prototype;
  while (proto) {
    if (proto === Error.prototype) return true;
    proto = Object.getPrototypeOf(proto);
  }
  return /^[A-Z]/.test(fn.name) && fn.name.endsWith('Error');
}

function expectedException(actual, expected, message, fn) {
  if (typeof expected === 'string') {
    const actualMessage = actual && actual.message !== undefined ? String(actual.message) : String(actual);
    if (String(actual) === expected || actualMessage === expected) {
      const isErrorMessage = actual && actual.message !== undefined;
      const err = new TypeError(
        isErrorMessage
          ? `The "error/message" argument is ambiguous. The error message "${expected}" is identical to the message.`
          : `The "error/message" argument is ambiguous. The error "${expected}" is identical to the message.`
      );
      err.code = 'ERR_AMBIGUOUS_ARGUMENT';
      throw err;
    }
    throw new AssertionError({
      actual, expected: undefined, operator: fn.name,
      message: `Got unwanted exception${expected ? `: ${expected}` : ''}`,
      stackStartFn: fn,
    });
  }
  if (expected === undefined) return true;
  if (typeof expected === 'function' && isErrorConstructor(expected)) {
    if (actual instanceof expected) return true;
    const received = actual && actual.constructor ? actual.constructor.name : typeof actual;
    const actualMessage = actual && actual.message !== undefined ? String(actual.message) : String(actual);
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || `The error is expected to be an instance of "${expected.name}". Received "${received}"\n\nError message:\n\n${actualMessage}`,
      generatedMessage: message === undefined,
      stackStartFn: fn,
    });
  }
  if (expected instanceof RegExp) {
    const str = actual && typeof actual.message === 'string' ? actual.message : String(actual);
    if (expected.test(str)) return true;
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || `The input did not match the regular expression ${expected}. Input:\n\n${inspectValue(str)}\n`,
      generatedMessage: message === undefined,
      stackStartFn: fn,
    });
  }
  if (typeof expected === 'function') {
    if (expected.call({}, actual) === true) return true;
    throw new AssertionError({
      actual, expected, operator: fn.name,
      message: message || 'The validation function is expected to return "true".',
      stackStartFn: fn,
    });
  }
  if (typeof expected === 'object' && expected !== null) {
    if (!(expected instanceof Error) && Object.keys(expected).length === 0) {
      const err = new TypeError("The argument 'error' may not be an empty object. Received {}");
      err.code = 'ERR_INVALID_ARG_VALUE';
      throw err;
    }
    for (const key of Object.keys(expected)) {
      const want = expected[key];
      const got = actual ? actual[key] : undefined;
      const hasKey = actual != null && Reflect.has(actual, key);
      if (want instanceof RegExp) {
        if (!want.test(String(got))) {
          throw new AssertionError({
            actual, expected, operator: fn.name,
            message: message || errorObjectComparisonMessage(actual, expected),
            generatedMessage: message === undefined,
            stackStartFn: fn,
          });
        }
      } else if (!hasKey || !isDeepStrictEqual(got, want)) {
        throw new AssertionError({
          actual, expected, operator: fn.name,
          message: message || errorObjectComparisonMessage(actual, expected),
          generatedMessage: message === undefined,
          stackStartFn: fn,
        });
      }
    }
    if (expected instanceof Error) {
      if (actual && actual.name === expected.name && actual.message === expected.message) return true;
      throw new AssertionError({
        actual, expected, operator: fn.name,
        message: message || errorObjectComparisonMessage(actual, expected),
        generatedMessage: message === undefined,
        stackStartFn: fn,
      });
    }
    return true;
  }
  return true;
}

function errorObjectComparisonMessage(actual, expected) {
  if (actual === null || typeof actual !== 'object') {
    return createErrDiff(actual, expected, 'Expected values to be strictly deep-equal:');
  }
  return createErrDiff(
    errorComparison(actual),
    expectedErrorComparison(actual, expected),
    'Expected values to be strictly deep-equal:'
  );
}

function Comparison() {}

function errorComparison(value) {
  const out = new Comparison();
  if (value && Object.prototype.hasOwnProperty.call(value, 'code')) out.code = value.code;
  for (const key of Object.keys(value || {})) {
    if (key === 'stack' || key === 'code') continue;
    out[key] = value[key];
  }
  if (value && Object.prototype.hasOwnProperty.call(value, 'message')) out.message = value.message;
  else if (value && typeof value.message === 'string') out.message = value.message;
  if (value && Object.prototype.hasOwnProperty.call(value, 'name')) out.name = value.name;
  else if (value && typeof value.name === 'string') out.name = value.name;
  return out;
}

function expectedErrorComparison(actual, expected) {
  const out = errorComparison(expected);
  for (const key of Object.keys(expected || {})) {
    const want = expected[key];
    const got = actual ? actual[key] : undefined;
    if (want instanceof RegExp && want.test(String(got))) {
      out[key] = got;
    }
  }
  return out;
}

function validateExpectedErrorArg(expected) {
  if (expected === undefined) return;
  if (typeof expected === 'function') return;
  if (expected instanceof RegExp) return;
  if (typeof expected === 'object' && expected !== null) return;
  const err = new TypeError(
    'The "error" argument must be of type function or an instance of Error, RegExp, or Object.' +
      invalidArgTypeSuffix(expected)
  );
  err.code = 'ERR_INVALID_ARG_TYPE';
  throw err;
}

function throws(fn, ...rest) {
  if (typeof fn !== 'function') {
    const err = new TypeError(
      `The "fn" argument must be of type function.${invalidArgTypeSuffix(fn)}`
    );
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  let expected; let message;
  if (rest.length >= 1) {
    if (rest.length === 1 && typeof rest[0] === 'string') message = rest[0];
    else { expected = rest[0]; message = rest[1]; }
  }
  validateExpectedErrorArg(expected);
  let thrown;
  let didThrow = false;
  try { fn(); } catch (e) { didThrow = true; thrown = e; }
  if (!didThrow) {
    let missingMessage;
    if (message !== undefined) {
      missingMessage = expected && expected.name
        ? `Missing expected exception (${expected.name}): ${message}`
        : `Missing expected exception: ${message}`;
    } else if (expected && expected.name) {
      missingMessage = `Missing expected exception (${expected.name}).`;
    } else {
      missingMessage = 'Missing expected exception.';
    }
    throw new AssertionError({
      actual: undefined, expected, operator: 'throws',
      message: missingMessage,
      stackStartFn: throws,
    });
  }
  if (expected === undefined && message !== undefined) {
    maybeThrowAmbiguousMessage(thrown, message);
  }
  expectedException(thrown, expected, message, throws);
}

function maybeThrowAmbiguousMessage(actual, message) {
  const actualMessage = actual && actual.message !== undefined ? String(actual.message) : String(actual);
  if (String(actual) !== message && actualMessage !== message) return;
  const isErrorMessage = actual && actual.message !== undefined;
  const err = new TypeError(
    isErrorMessage
      ? `The "error/message" argument is ambiguous. The error message "${message}" is identical to the message.`
      : `The "error/message" argument is ambiguous. The error "${message}" is identical to the message.`
  );
  err.code = 'ERR_AMBIGUOUS_ARGUMENT';
  throw err;
}

function doesNotThrow(fn, ...rest) {
  if (typeof fn !== 'function') {
    const err = new TypeError(
      `The "fn" argument must be of type function.${invalidArgTypeSuffix(fn)}`
    );
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  let expected; let message;
  if (typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  if (expected !== undefined &&
      !(typeof expected === 'function' && isErrorConstructor(expected)) &&
      !(expected instanceof RegExp)) {
    const err = new TypeError(
      'The "expected" argument must be of type function or an instance of RegExp.' +
        invalidArgTypeSuffix(expected)
    );
    err.code = 'ERR_INVALID_ARG_TYPE';
    throw err;
  }
  let thrown; let didThrow = false;
  try { fn(); } catch (e) { didThrow = true; thrown = e; }
  if (didThrow) {
    if (expected && !matchesFilter(thrown, expected)) throw thrown;
    const actualMessage = thrown && thrown.message !== undefined ? String(thrown.message) : String(thrown);
    const unwanted = message !== undefined
      ? `Got unwanted exception: ${message}`
      : 'Got unwanted exception.';
    throw new AssertionError({
      actual: thrown, expected, operator: 'doesNotThrow',
      message: `${unwanted}\nActual message: "${actualMessage}"`,
      stackStartFn: doesNotThrow,
    });
  }
}

function matchesFilter(actual, expected) {
  if (typeof expected === 'function' && isErrorConstructor(expected)) return actual instanceof expected;
  if (expected instanceof RegExp) return expected.test(actual && actual.message ? actual.message : String(actual));
  return true;
}

async function rejects(promiseFn, ...rest) {
  let expected; let message;
  if (rest.length === 1 && typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  validateExpectedErrorArg(expected);
  let thrown; let didReject = false;
  try {
    const p = typeof promiseFn === 'function' ? promiseFn() : promiseFn;
    await p;
  } catch (e) { didReject = true; thrown = e; }
  if (!didReject) {
    throw new AssertionError({
      actual: undefined, expected, operator: 'rejects',
      message: message ? `Missing expected rejection: ${message}` : 'Missing expected rejection.',
      stackStartFn: rejects,
    });
  }
  expectedException(thrown, expected, message, rejects);
}

async function doesNotReject(promiseFn, ...rest) {
  let expected; let message;
  if (typeof rest[0] === 'string') message = rest[0];
  else { expected = rest[0]; message = rest[1]; }
  let thrown; let didReject = false;
  try {
    const p = typeof promiseFn === 'function' ? promiseFn() : promiseFn;
    await p;
  } catch (e) { didReject = true; thrown = e; }
  if (didReject) {
    if (expected && !matchesFilter(thrown, expected)) throw thrown;
    throw new AssertionError({
      actual: thrown, expected, operator: 'doesNotReject',
      message: `Got unwanted rejection.\n${thrown && thrown.message ? thrown.message : ''}`,
      stackStartFn: doesNotReject,
    });
  }
}

function match(string, regexp, message) {
  if (!(regexp instanceof RegExp)) {
    throw regexpArgError(regexp);
  }
  if (typeof string !== 'string') {
    throw new AssertionError({ actual: string, expected: regexp, operator: 'match', message, stackStartFn: match });
  }
  if (!regexp.test(string)) {
    innerFail({ actual: string, expected: regexp, message, operator: 'match', stackStartFn: match });
  }
}
function doesNotMatch(string, regexp, message) {
  if (!(regexp instanceof RegExp)) {
    throw regexpArgError(regexp);
  }
  if (typeof string === 'string' && regexp.test(string)) {
    innerFail({ actual: string, expected: regexp, message, operator: 'doesNotMatch', stackStartFn: doesNotMatch });
  }
}

const CallTracker = makeCallTracker(AssertionError);

// ---- assemble the standalone surface ----
function assignMethods(target) {
  target.ok = ok;
  target.equal = equal;
  target.notEqual = notEqual;
  target.strictEqual = strictEqual;
  target.notStrictEqual = notStrictEqual;
  target.deepEqual = deepEqual;
  target.notDeepEqual = notDeepEqual;
  target.deepStrictEqual = deepStrictEqual;
  target.notDeepStrictEqual = notDeepStrictEqual;
  target.partialDeepStrictEqual = partialDeepStrictEqual;
  target.throws = throws;
  target.doesNotThrow = doesNotThrow;
  target.rejects = rejects;
  target.doesNotReject = doesNotReject;
  target.ifError = ifError;
  target.fail = fail;
  target.match = match;
  target.doesNotMatch = doesNotMatch;
  return target;
}

function assignStrictMethods(target) {
  assignMethods(target);
  target.equal = strictEqual;
  target.notEqual = notStrictEqual;
  target.deepEqual = deepStrictEqual;
  target.notDeepEqual = notDeepStrictEqual;
  return target;
}

// §Assert — the constructible form. `new Assert({ strict, diff })` yields an
// object carrying the assertion methods; calling without `new` throws
// ERR_CONSTRUCT_CALL_REQUIRED (a plain function, not an ES class, so the code
// is ours to set rather than the engine's bare "requires new").
const kValidDiff = new Set([undefined, 'simple', 'full']);

function Assert(options) {
  if (!(this instanceof Assert)) {
    const e = new TypeError("Class constructor Assert cannot be invoked without 'new'");
    e.code = 'ERR_CONSTRUCT_CALL_REQUIRED';
    throw e;
  }
  options = options || {};
  const diffMode = options.diff;
  if (!kValidDiff.has(diffMode)) {
    const e = new TypeError(
      `The property 'options.diff' must be one of: 'simple', 'full'. ` +
        `Received ${inspectValue(diffMode)}`
    );
    e.code = 'ERR_INVALID_ARG_VALUE';
    throw e;
  }
  const strict = options.strict !== false;
  const skipPrototype = options.skipPrototype === true;
  this.strict = strict;

  // Tag any AssertionError thrown through this instance with its diff mode.
  const wrap = diffMode === undefined ? (fn) => fn : (fn) => function wrapped(...args) {
    try { return fn.apply(this, args); } catch (e) {
      if (e instanceof AssertionError && e.diff === undefined) e.diff = diffMode;
      throw e;
    }
  };

  // Strict instances alias the loose comparators to the strict ones (so
  // `instance.equal === instance.strictEqual`).
  const strictEqualM = wrap(strictEqual);
  const notStrictEqualM = wrap(notStrictEqual);
  const deepStrictEqualM = wrap(skipPrototype ? deepStrictEqualSkipPrototype : deepStrictEqual);
  const notDeepStrictEqualM = wrap(skipPrototype ? notDeepStrictEqualSkipPrototype : notDeepStrictEqual);

  this.ok = wrap(ok);
  this.strictEqual = strictEqualM;
  this.notStrictEqual = notStrictEqualM;
  this.deepStrictEqual = deepStrictEqualM;
  this.notDeepStrictEqual = notDeepStrictEqualM;
  this.equal = strict ? strictEqualM : wrap(equal);
  this.notEqual = strict ? notStrictEqualM : wrap(notEqual);
  this.deepEqual = strict ? deepStrictEqualM : wrap(deepEqual);
  this.notDeepEqual = strict ? notDeepStrictEqualM : wrap(notDeepEqual);
  this.partialDeepStrictEqual = wrap(partialDeepStrictEqual);
  this.throws = throws;
  this.doesNotThrow = doesNotThrow;
  this.rejects = rejects;
  this.doesNotReject = doesNotReject;
  this.ifError = ifError;
  this.fail = wrap(fail);
  this.match = match;
  this.doesNotMatch = doesNotMatch;
  this.AssertionError = AssertionError;
  this.CallTracker = CallTracker;
}
Assert.prototype.constructor = Assert;

assignMethods(assert);
assert.assert = assert;
assert.AssertionError = AssertionError;
assert.CallTracker = CallTracker;
assert.Assert = Assert;
const strict = assignStrictMethods(function strictAssert(...args) { ok(...args); });
strict.assert = strict;
strict.AssertionError = AssertionError;
strict.CallTracker = CallTracker;
strict.Assert = Assert;
strict.strict = strict;
assert.strict = strict;
if (typeof globalThis !== 'undefined') globalThis[kAssertCache] = assert;

module.exports = assert;
