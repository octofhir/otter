// new Function with repeated sources: each call is a fresh function object
// with its own prototype and template objects; errors stay errors; params and
// body are told apart; generator/async kinds; toString and properties.
var out = [];
function log() { out.push(Array.prototype.join.call(arguments, ' ')); }
var fs = [];
for (var i = 0; i < 5; i++) fs.push(new Function('a', 'b', 'return a + b'));
log('distinct', fs[0] !== fs[1], fs[0].prototype !== fs[1].prototype, fs[3](2, 3));
log('toString', JSON.stringify(fs[2].toString()), fs[2].name, fs[2].length);
fs[0].x = 1; log('own props', fs[1].x, Object.keys(fs[0]).join(','));
var tags = [];
function tag(t) { tags.push(t); return t; }
globalThis.tag = tag;
for (var i = 0; i < 3; i++) new Function("return tag`a${1}b`")();
var tf = new Function('return tag`a${1}b`');
tf(); tf();
log('templates', tags.length, tags[0] !== tags[1], tags[1] !== tags[2], tags[3] === tags[4]);
log('split', new Function('a,b', 'return b')(1, 2), new Function('a', 'b', 'return b')(1, 2));
try { new Function('a', '}'); log('no error'); } catch (e) { log('err1', e.name); }
try { new Function('a', '}'); log('no error'); } catch (e) { log('err2', e.name); }
try { new Function('/*', '*/){'); log('smuggled'); } catch (e) { log('err3', e.name); }
var G = Object.getPrototypeOf(function* () {}).constructor;
var g1 = new G('yield 1; yield 2'), g2 = new G('yield 1; yield 2');
log('gen', g1 !== g2, Array.from(g1()).join(','), Array.from(g2()).join(','));
var A = Object.getPrototypeOf(async function () {}).constructor;
var a1 = new A('return 7'), a2 = new A('return 7');
log('async', a1 !== a2, Object.getPrototypeOf(a1) === Object.getPrototypeOf(a2));
var counters = [];
for (var i = 0; i < 3; i++) counters.push(new Function('var n = 0; return function () { return ++n; }')());
counters[0](); counters[0]();
log('closures', counters[0](), counters[1](), counters[2]());
var re = [];
for (var i = 0; i < 2; i++) re.push(new Function('return /x/g')());
log('regexp', re[0] !== re[1]);
var pf = [];
for (var i = 0; i < 2; i++) pf.push(new Function('return function inner() {}')());
log('inner', pf[0] !== pf[1], pf[0].prototype !== pf[1].prototype, pf[0].name);
var hot = 0;
for (var i = 0; i < 3000; i++) hot += new Function('x', 'return x * 2 + ' + (i % 7))(i);
log('hot', hot);
console.log(out.join('\n'));
