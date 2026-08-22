'use strict';

// The ESM loader the module mocker reaches for.
//
// Mocking a module means intercepting how one is resolved and loaded, which
// this runtime does not let a program do. Nothing else in the test runner asks
// for the loader, so it is only reached by `mock.module()` — and that is where
// it says so, rather than at load time where it would take the whole runner
// with it.
const {
  codes: { ERR_METHOD_NOT_IMPLEMENTED },
} = require('internal/errors');

function getOrInitializeCascadedLoader() {
  throw new ERR_METHOD_NOT_IMPLEMENTED('mocking a module');
}

module.exports = { getOrInitializeCascadedLoader };
