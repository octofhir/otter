const test = require('node:test');
test('fails', () => { throw new Error('boom'); });
