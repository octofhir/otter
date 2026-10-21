require.cache['internal/querystring'] = { exports: { encodeStr: () => 'POISONED', hexTable: [], isHexTable: [] }, loaded: true };
const qs = require('node:querystring');
console.log(qs.escape('a b'));
