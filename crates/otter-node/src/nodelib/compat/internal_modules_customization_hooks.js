'use strict';

// Load hooks, which this runtime does not let a program register. Only the
// module mocker asks for them, and only when it is used.
const {
  codes: { ERR_METHOD_NOT_IMPLEMENTED },
} = require('internal/errors');

function registerHooks() {
  throw new ERR_METHOD_NOT_IMPLEMENTED('registering module hooks');
}

module.exports = { registerHooks };
