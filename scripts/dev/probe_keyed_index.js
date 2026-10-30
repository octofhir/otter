// Keyed reads and writes with integer keys on ordinary objects, strings,
// prototypes and arrays: tables indexed by codes, absent keys, inherited
// integer keys, string characters past the end, non-Latin-1 units.
var out = [];
var table = {}; for (var i = 0; i < 300; i++) table[i * 3] = 'v' + i;
var hit = 0, miss = 0;
for (var r = 0; r < 20; r++) for (var i = 0; i < 900; i++) { if (table[i] !== undefined) hit++; else miss++; }
out.push(hit, miss, table[897], table['3'], table[3.0], table[-0]);
var proto = { 7: 'inherited' }; var child = Object.create(proto); child[1] = 'own';
out.push(child[7], child[1], child[2]);
var s = 'héllo €'; var chars = []; for (var i = -1; i < 9; i++) chars.push(s[i]); out.push(chars.join('|'));
String.prototype[20] = 'proto-char'; out.push(s[20], 'ab'[1]); delete String.prototype[20];
var big = {}; big[4294967294] = 'x'; big[2147483648] = 'y'; out.push(big[4294967294], big[2147483648], big[2147483647]);
var a = [1, 2]; a.extra = 1; out.push(a[1], a[5], a['extra']);
var lex = 'function(a,b){return a+b}', n = 0; for (var k = 0; k < 50; k++) for (var i = 0; i < lex.length; i++) if (lex[i] === 'a') n++; out.push(n);
console.log(out.join('\n'));
