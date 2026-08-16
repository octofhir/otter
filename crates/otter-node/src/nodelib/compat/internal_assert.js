'use strict';
function assert(value, message) {
  if (!value) {
    const err = new Error(message ?? 'Assertion failed');
    err.code = 'ERR_INTERNAL_ASSERTION';
    throw err;
  }
}
assert.fail = function fail(message) { assert(false, message); };
module.exports = assert;
