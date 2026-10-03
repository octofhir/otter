// A method call site with many receiver shapes: each shape's method runs
// inline, a shape seen only later takes the generic path, and a prototype
// method replaced mid-run is called afresh.
class A { f(x) { return x + 1; } }
class B { f(x) { return x * 2; } }
class C { f(x) { return x - 3; } }
class D { f(x) { return this.k + x; } constructor() { this.k = 7; } }
class E extends A { f(x) { return super.f(x) + 10; } }
const objs = [new A(), new B(), new C(), new D(), new E()];
function run(list, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + list[i % list.length].f(i)) | 0;
  return s;
}
let total = 0;
for (let r = 0; r < 30; r++) total = (total + run(objs, 4000)) | 0;
class Late { f(x) { return x ^ 5; } }
total = (total + run([...objs, new Late()], 4000)) | 0;
B.prototype.f = function (x) { return x + 100; };
total = (total + run(objs, 4000)) | 0;
const plain = { f(x) { return x & 3; } };
total = (total + run([plain, ...objs], 4000)) | 0;
console.log(total);
