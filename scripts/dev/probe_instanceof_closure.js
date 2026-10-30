// instanceof with ordinary-function targets: own and inherited
// @@hasInstance, [[Prototype]] changes, reassigned/primitive prototypes,
// bound functions, proxies and primitives on the left.
var out = [];
function log(label, f) { try { out.push(label + ' ' + f()); } catch (e) { out.push(label + ' throws ' + e.constructor.name); } }
function A() {} function B() {} B.prototype = Object.create(A.prototype);
var b = new B();
log('plain', function () { return [b instanceof A, b instanceof B, {} instanceof A, 1 instanceof A].join(); });
function C() {} C.x = 1; C.y = 2; C.z = 3;
log('props', function () { return new C() instanceof C; });
function D() {} Object.defineProperty(D, Symbol.hasInstance, { value: function (v) { return v === 42; } });
log('own hasInstance', function () { return [42 instanceof D, new D() instanceof D].join(); });
function E() {} var parent = { [Symbol.hasInstance]: function () { return true; } };
Object.setPrototypeOf(E, parent);
log('proto hasInstance', function () { return [1 instanceof E, {} instanceof E].join(); });
function F() {} Object.setPrototypeOf(F, D);
log('proto function', function () { return [42 instanceof F, new F() instanceof F].join(); });
function G() {} Object.setPrototypeOf(G, null);
log('null proto', function () { return {} instanceof G; });
function H() {} H.prototype = 3;
log('primitive prototype', function () { return {} instanceof H; });
function I() {} var i1 = new I(); I.prototype = {};
log('reassigned', function () { return [i1 instanceof I, new I() instanceof I].join(); });
var Bound = A.bind(null);
log('bound', function () { return [b instanceof Bound, {} instanceof Bound].join(); });
log('proxy lhs', function () { return new Proxy(b, {}) instanceof A; });
log('proxy rhs', function () { return b instanceof new Proxy(A, {}); });
var getterCalls = 0;
function J() {} Object.defineProperty(J, Symbol.hasInstance, { get: function () { getterCalls++; return undefined; } });
log('getter undefined', function () { return [new J() instanceof J, getterCalls].join(); });
var arrowish = function () {};
log('expr fn', function () { return new arrowish() instanceof arrowish; });
var gen = function* () {};
log('generator', function () { return gen() instanceof gen; });
var hot = 0;
var ctors = [A, B, C, I];
for (var k = 0; k < 20000; k++) hot += (b instanceof ctors[k % 4]) ? 1 : 0;
out.push('hot ' + hot);
console.log(out.join('\n'));
