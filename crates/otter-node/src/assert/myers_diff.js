'use strict';
// internal/assert/myers_diff — Myers O(ND) difference algorithm used to render
// the actual/expected diff in AssertionError messages. Mirrors Node's
// lib/internal/assert/myers_diff.js surface: `myersDiff(actual, expected)`
// returns an edit script of { type: -1|0|1, value } entries (remove/keep/add).

const kNopLinesToCollapse = 5;

function outOfRange(value) {
  const e = new RangeError(
    'The value of "myersDiff input size" is out of range. ' +
      `It must be < 2^31. Received ${value}`
  );
  e.code = 'ERR_OUT_OF_RANGE';
  return e;
}

// Compute the shortest edit script between two sequences (arrays / array-likes
// compared with ===). Throws ERR_OUT_OF_RANGE when the combined size would
// overflow the 32-bit V-array indices Node uses.
function myersDiff(actual, expected) {
  const actualLength = actual.length;
  const expectedLength = expected.length;
  const max = actualLength + expectedLength;
  if (max >= 2 ** 31) {
    throw outOfRange(max);
  }

  const v = new Array(2 * max + 1).fill(0);
  const trace = [];

  for (let diffLevel = 0; diffLevel <= max; diffLevel++) {
    trace.push(v.slice());
    for (let k = -diffLevel; k <= diffLevel; k += 2) {
      let x;
      if (k === -diffLevel || (k !== diffLevel && v[k - 1 + max] < v[k + 1 + max])) {
        x = v[k + 1 + max];
      } else {
        x = v[k - 1 + max] + 1;
      }
      let y = x - k;
      while (x < actualLength && y < expectedLength && actual[x] === expected[y]) {
        x++;
        y++;
      }
      v[k + max] = x;
      if (x >= actualLength && y >= expectedLength) {
        return backtrack(trace, actual, expected, max);
      }
    }
  }
  return [];
}

function backtrack(trace, actual, expected, max) {
  const result = [];
  let x = actual.length;
  let y = expected.length;
  for (let diffLevel = trace.length - 1; diffLevel >= 0; diffLevel--) {
    const v = trace[diffLevel];
    const k = x - y;
    let prevK;
    if (k === -diffLevel || (k !== diffLevel && v[k - 1 + max] < v[k + 1 + max])) {
      prevK = k + 1;
    } else {
      prevK = k - 1;
    }
    const prevX = v[prevK + max];
    const prevY = prevX - prevK;
    while (x > prevX && y > prevY) {
      result.push({ type: 0, value: actual[x - 1] });
      x--;
      y--;
    }
    if (diffLevel > 0) {
      if (x > prevX) {
        result.push({ type: -1, value: actual[x - 1] });
        x--;
      } else {
        result.push({ type: 1, value: expected[y - 1] });
        y--;
      }
    }
  }
  result.reverse();
  return result;
}

module.exports = { myersDiff, kNopLinesToCollapse };
