function loose(a, b) { return a == b; }
function strict(a, b) { return a === b; }
const o = {};
const vals = [NaN, 1, 0, -0, 1.5, -1, 2147483647, 1e300, 'ab', '', 10n, o, undefined, null, true];
let r = '';
for (let i = 0; i < 3000; i++) { r = ''; for (const a of vals) { for (const b of vals) r += (loose(a, b) ? 1 : 0) + '' + (strict(a, b) ? 1 : 0); r += '\n'; } }
console.log(r);
