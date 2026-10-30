// Own accessors read and written through hot sites: object-literal getters
// and setters, same-shape objects with different getters, redefinition,
// deletion back to data, and receivers of several shapes.
var out = [];
function make(k) { return { get v() { return k; }, set v(x) { this.w = x * k; }, w: 0 }; }
var a = make(2), b = make(3);
var c = { get v() { return 'c'; }, set v(x) { this.w = 'C' + x; }, w: 0 };
function read(o) { return o.v; }
function write(o, x) { o.v = x; return o.w; }
var s = 0, last;
for (var i = 0; i < 30000; i++) { s += read(i & 1 ? a : b); last = write(i & 1 ? a : b, i); }
out.push(s, last, read(c), write(c, 5));
Object.defineProperty(a, 'v', { get: function () { return 100; }, set: function (x) { this.w = -x; }, configurable: true });
var t = 0; for (var i = 0; i < 1000; i++) { t += read(a); write(a, i); } out.push(t, a.w);
delete b.v; b.v = 7; var u = 0; for (var i = 0; i < 1000; i++) u += read(b); out.push(u, write(b, 9), b.v);
var shapes = [];
for (var i = 0; i < 6; i++) { var o = make(i); o['x' + i] = i; shapes.push(o); }
var m = 0; for (var i = 0; i < 6000; i++) m += read(shapes[i % 6]); out.push(m);
var counter = { n: 0, get next() { return ++this.n; } }; var z = 0; for (var i = 0; i < 1000; i++) z += counter.next; out.push(z, counter.n);
console.log(out.join('\n'));
