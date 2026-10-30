// Object.prototype.hasOwnProperty and toString over the receivers type
// checks and own-key tests use: shaped, dictionary, numeric and unicode
// keys, never-interned keys, inherited and deleted properties, every
// builtin tag and a custom @@toStringTag.
var out = [];
var has = Object.prototype.hasOwnProperty, str = Object.prototype.toString;
var o = { a: 1, '0': 2, 'ключ': 3 }; var dict = {}; for (var i = 0; i < 40; i++) dict['k' + i] = i;
delete dict.k5;
var child = Object.create(o); child.own = 1;
var built = 'dyn' + Math.floor(Math.random() * 0 + 7);
out.push(has.call(o, 'a'), has.call(o, '0'), has.call(o, 0), has.call(o, 'ключ'), has.call(o, 'zz' + built),
  has.call(dict, 'k6'), has.call(dict, 'k5'), has.call(child, 'a'), has.call(child, 'own'), has.call([1, 2], '1'),
  has.call([1, 2], 'length'), has.call(function f() {}, 'name'), has.call('abc', '1'));
var tags = [[], {}, null, undefined, 1, 'x', true, function () {}, new Date(0), /x/, new Error('e'),
  (function () { return arguments; })(), new Map(), new Set(), Symbol('s'), 1n, Promise.resolve(), new Uint8Array(1), JSON, Math];
out.push(tags.map(function (t) { return str.call(t); }).join(','));
var custom = { [Symbol.toStringTag]: 'Custom' }; out.push(str.call(custom), String(custom));
class K { get [Symbol.toStringTag]() { return 'K!'; } } out.push(str.call(new K()));
var counts = {}; for (var i = 0; i < 20000; i++) { var t = str.call(tags[i % tags.length]); counts[t] = (counts[t] | 0) + 1; }
out.push(Object.keys(counts).length, counts['[object Array]']);
var hits = 0; for (var i = 0; i < 20000; i++) if (has.call(dict, 'k' + (i % 50))) hits++;
out.push(hits);
function fn() {} fn.custom = 1; var arrowf = () => 0; var namedLen = function named(a, b) {}; delete namedLen.name;
out.push([has.call(fn, 'custom'), has.call(fn, 'prototype'), has.call(fn, 'name'), has.call(fn, 'length'), has.call(fn, 'nope'),
  has.call(arrowf, 'prototype'), has.call(namedLen, 'name'), has.call(namedLen, 'length'), has.call(fn, 'zz' + built), has.call(class Q {}, 'prototype')].join(','));
var plain = Object.create(null); var tagged = Object.create({ [Symbol.toStringTag]: 'Inherited' });
out.push(str.call(plain), str.call(tagged), str.call(Object.create(tagged)));
Object.defineProperty(String.prototype, Symbol.toStringTag, { value: 'Str', configurable: true }); out.push(str.call('x'), str.call(new String('y')));
delete String.prototype[Symbol.toStringTag];
Object.defineProperty(Number.prototype, Symbol.toStringTag, { get: function () { return typeof this; }, configurable: true }); out.push(str.call(5));
delete Number.prototype[Symbol.toStringTag]; out.push(str.call(5), str.call(true));
console.log(out.join('\n'));
