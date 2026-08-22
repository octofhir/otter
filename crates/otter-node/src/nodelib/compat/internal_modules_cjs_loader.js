'use strict';

// The CommonJS loader, as the vendored sources reach for it. What this runtime
// exposes of it is what `module` carries; the members only the module mocker
// uses are absent, and it is that mocker which reports so when asked to work.
module.exports = { Module: require('module') };
