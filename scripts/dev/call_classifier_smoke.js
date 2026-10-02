// Callee classification smoke: every callable kind through call and construct.
const log = [];
function sloppy() { return this === globalThis; }
function strict() { "use strict"; return this; }
log.push(sloppy(), strict() === undefined, sloppy.call(null), typeof strict.call(5));
const prim = function () { return typeof this; };
log.push(prim.call(7), prim.call("s"));
const bound = function (a, b, c) { return [this.tag, a, b, c].join(","); }.bind({ tag: "T" }, 1);
log.push(bound(2, 3));
const bound2 = bound.bind(null, 9);
log.push(bound2(8));
class Base { constructor(x) { this.x = x; } }
class Derived extends Base { constructor(x) { super(x * 2); this.y = 1; } }
const d = new Derived(4);
log.push(d.x, d.y, d instanceof Derived, d instanceof Base);
try { Base(); } catch (e) { log.push(e.constructor.name); }
const BoundBase = Base.bind(null, 11);
const bb = new BoundBase();
log.push(bb.x, bb instanceof Base);
function F(a) { this.a = a; }
F.prototype.get = function () { return this.a; };
log.push(new F(3).get());
function R() { return { r: 1 }; }
log.push(new R().r);
log.push(Math.max(1, 5, 3), [3, 1, 2].map(x => x * 2).join(""));
log.push(Array.prototype.join.call([1, 2], "-"));
log.push(Math.max.apply(null, [4, 9, 2]));
const p = new Proxy(function (a) { return a + 1; }, { apply(t, th, args) { return t(...args) * 10; } });
log.push(p(1));
const p2 = new Proxy(function () {}, {});
log.push(typeof p2());
const PC = new Proxy(F, { construct(t, args) { return { via: args[0] }; } });
log.push(new PC(5).via);
function* gen(a) { yield a; yield a + 1; }
const g = gen(10);
log.push(g.next().value, g.next().value, Object.getPrototypeOf(g) === gen.prototype);
async function af(v) { await null; return v * 2; }
const pr = af(21);
log.push(pr instanceof Promise);
pr.then(v => { log.push(v); console.log(log.join("|")); });
const arrow = () => this;
log.push(arrow.call(5) === this);
function spread(...xs) { return xs.length; }
log.push(spread(...[1, 2, 3, 4]));
try { (void 0)(); } catch (e) { log.push(e instanceof TypeError); }
try { new (() => 1)(); } catch (e) { log.push(e instanceof TypeError); }
const o = { m(x) { return this === o && x; } };
log.push(o.m(true));
log.push([1, 2, 3].reduce((a, b) => a + b));
log.push(String(Symbol("q").description));
