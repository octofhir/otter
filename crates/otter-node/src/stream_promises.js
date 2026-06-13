'use strict';
// `node:stream/promises` — promise-returning finished/pipeline.
const stream = require('stream');
module.exports = { finished: stream.finished, pipeline: stream.pipeline };
