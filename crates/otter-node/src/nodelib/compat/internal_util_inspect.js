'use strict';
const util = require('util');
module.exports = {
  inspect: util.inspect,
  format: util.format,
  formatWithOptions: util.formatWithOptions ??
    ((options, ...args) => util.format(...args)),
};
