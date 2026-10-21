const before = new Set(Object.keys(require.cache));
require('querystring');
const added = Object.keys(require.cache).filter(k => !before.has(k));
console.log('querystring pulled', added);
