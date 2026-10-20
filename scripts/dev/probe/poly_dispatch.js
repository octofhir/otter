// Deltablue-shaped kernels: prototype-inherited polymorphic method calls,
// `this` field traffic and an array-backed collection.
function Base(v) { this.v = v; this.mark = 0; }
Base.prototype.isSatisfied = function () { return this.mark !== 0; };
Base.prototype.output = function () { return this.v; };
function A(v) { Base.call(this, v); this.a = 1; }
A.prototype = Object.create(Base.prototype);
A.prototype.execute = function () { this.v.value = this.v.value + this.a; };
function B(v) { Base.call(this, v); this.b = 2; }
B.prototype = Object.create(Base.prototype);
B.prototype.execute = function () { this.v.value = this.v.value * this.b % 1000; };
function C(v) { Base.call(this, v); this.c = 3; this.d = 4; }
C.prototype = Object.create(Base.prototype);
C.prototype.execute = function () { this.v.value = (this.v.value + this.c) - this.d; };
C.prototype.isSatisfied = function () { return true; };
function D(v) { Base.call(this, v); }
D.prototype = Object.create(Base.prototype);
D.prototype.execute = function () { this.v.value = this.v.value ^ 1; };
function Coll() { this.elms = []; }
Coll.prototype.add = function (e) { this.elms.push(e); };
Coll.prototype.at = function (i) { return this.elms[i]; };
Coll.prototype.size = function () { return this.elms.length; };
var plan = new Coll();
for (var i = 0; i < 64; i++) {
  var v = { value: i };
  var k = i % 4;
  plan.add(k === 0 ? new A(v) : k === 1 ? new B(v) : k === 2 ? new C(v) : new D(v));
}
function run(n) {
  var s = 0;
  for (var r = 0; r < n; r++) {
    for (var i = 0; i < plan.size(); i++) {
      var c = plan.at(i);
      c.execute();
      if (c.isSatisfied()) s++;
      s += c.output().value & 1;
    }
  }
  return s;
}
var t0 = Date.now();
console.log(run(200000), Date.now() - t0, "ms");
