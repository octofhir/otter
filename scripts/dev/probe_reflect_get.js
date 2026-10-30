// Reflect.get with string keys, interned or built at run time, on ordinary,
// inherited, accessor (with receiver), array, String wrapper, typed array,
// proxy and frozen targets, numeric-looking keys, symbols, non-object
// targets, and the proxyGetter pattern in a hot loop.
var out = [];
var proto = { inherited: 'I', get acc() { return 'acc:' + this.tag; } };
var o = Object.create(proto); o.own = 'O'; o.tag = 't';
var built = 'ow' + 'n', sym = Symbol('s'); o[sym] = 'S';
var arr = [10, 20, 30]; var sw = new String('hey'); var ta = new Uint8Array([7, 8]);
var px = new Proxy(o, { get: function (t, k, r) { return typeof k === 'string' ? 'P:' + k : 'P:sym'; } });
out.push([Reflect.get(o, 'own'), Reflect.get(o, built), Reflect.get(o, 'inherited'), Reflect.get(o, 'acc'), Reflect.get(o, 'acc', { tag: 'R' }), Reflect.get(o, 'missing' + ''), Reflect.get(o, sym)].join('|'));
out.push([Reflect.get(arr, '1'), Reflect.get(arr, 'length'), Reflect.get(arr, 1), Reflect.get(sw, '0'), Reflect.get(sw, 'length'), Reflect.get(ta, '1'), Reflect.get(ta, '-0'), Reflect.get(ta, '1.5')].map(String).join('|'));
out.push([Reflect.get(px, 'own'), Reflect.get(px, sym), Reflect.get(Object.freeze({ f: 1 }), 'f'), Reflect.get(function g() {}, 'name')].join('|'));
try { Reflect.get(1, 'x'); } catch (e) { out.push('prim ' + e.constructor.name); }
var target = { a: 1, b: 2, c: 3 };
var proxied = new Proxy(target, { get: function (t, k) { return Reflect.get(t, k); } });
var hot = 0; var names = ['a', 'b', 'c', 'd'];
for (var i = 0; i < 40000; i++) hot += (proxied[names[i & 3]] | 0);
out.push('hot ' + hot);
console.log(out.join('\n'));
