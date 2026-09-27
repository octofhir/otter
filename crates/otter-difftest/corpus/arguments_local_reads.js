// Non-escaping arguments aliases read the activation window. A cold property
// access materializes one identity, whose mutations survive later reads and GC.
function sum() {
  var a = arguments, b = a, result = 0;
  for (var i = 0; i < b.length; ++i) result += b[i].n;
  return result;
}
var total = 0;
for (var i = 0; i < 256; ++i) total += sum({n: i}, {n: 3}, {n: 7});
console.log(total, sum(), sum({n: 9}));
var key = 0, coercions = 0;
function read() { var a = arguments; return a[key]; }
for (var i = 0; i < 256; ++i) read(19);
var keys = [0, -0, '0', 0.5, -1, 4294967296, NaN, Symbol('absent')];
for (var i = 0; i < keys.length; ++i) { key = keys[i]; console.log(String(read(19))); }
key = { toString: function () { ++coercions; return '0'; } };
console.log(read(23), coercions);
var saved;
Object.defineProperty(Object.prototype, '-1', {
  configurable: true,
  get: function () { saved = this; this[0] = {n: 41}; this.length = 5; return 11; }
});
function mutate() {
  var a = arguments;
  var before = a[0].n;
  var miss = a[-1];
  var junk = [];
  for (var i = 0; i < 8; ++i) junk.push({n: i});
  return before + ':' + miss + ':' + a[0].n + ':' + a.length;
}
for (var i = 0; i < 64; ++i) mutate({n: 7});
console.log(mutate({n: 3}), saved[0].n, saved.length);
delete Object.prototype['-1'];
var late = 0;
function lateGetter() { var a = arguments; var v = a[late]; return String(v) + ':' + a.length + ':' + a[0]; }
for (var i = 0; i < 256; ++i) lateGetter(2);
Object.defineProperty(Object.prototype, '8', {
  configurable: true,
  get: function () { this.length = 9; this[0] = 31; return 17; }
});
late = 8;
console.log(lateGetter(2));
delete Object.prototype['8'];
function escape() { var a = arguments; return a; }
console.log(escape(13)[0]);
function mapped(x) { var a = arguments; x = 29; return a[0]; }
console.log(mapped(1));
function captured() { var a = arguments; return function () { return a[0]; }; }
console.log(captured(37)());
function dynamic() { var a = arguments; eval('a[0] = 43'); return a[0]; }
console.log(dynamic(1));
function mixed(flag) { var a = arguments; if (flag) a = [3, 5]; return a.length + ':' + a[0]; }
console.log(mixed(false), mixed(true));
var originalValues = Array.prototype.values;
function intrinsic() {
  var a = arguments;
  Array.prototype.values = function replaced() {};
  return a[Symbol.iterator] === originalValues;
}
console.log(intrinsic(1), escape(1)[Symbol.iterator] === originalValues);
Array.prototype.values = originalValues;
function throwing() { return arguments[key]; }
key = { toString: function () { throw new Error('once'); } };
try { throwing(1); } catch (e) { console.log(e.message); }
