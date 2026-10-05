// Megamorphic method call site: eight receiver classes, one `o.f(i)` site.
class A { f(x) { return x + 1; } }
class B { f(x) { return x + 2; } }
class C { f(x) { return x + 3; } }
class D { f(x) { return x + 4; } }
class E { f(x) { return x + 5; } }
class F { f(x) { return x + 6; } }
class G { f(x) { return x + 7; } }
class H { f(x) { return x + 8; } }
const objs = [new A(), new B(), new C(), new D(), new E(), new F(), new G(), new H()];
function run(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + objs[i & 7].f(i)) | 0;
  return s;
}
let total = 0;
for (let r = 0; r < 20; r++) total = (total + run(1000000)) | 0;
console.log(total);
