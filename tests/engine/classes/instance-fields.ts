/* otter-test:
name = "classes: public instance fields run before constructor body"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
class Point {
  x = 0;
  y = 0;
  count = 0;
  constructor(x: number, y: number) {
    // Instance fields are already set when the body runs.
    if (this.x !== 0) fail();
    if (this.y !== 0) fail();
    this.x = x;
    this.y = y;
  }
}
let p = new Point(3, 4);
if (p.x !== 3) fail();
if (p.y !== 4) fail();
if (p.count !== 0) fail();

class WithoutCtor {
  greeting = "hi";
  size = 1 + 2;
}
let w = new WithoutCtor();
if (w.greeting !== "hi") fail();
if (w.size !== 3) fail();

// Field initializers can reference earlier fields and outer scope.
let scale = 10;
class Scaled {
  base = 5;
  doubled = this.base * 2;
  outerScale = scale * 3;
}
let s = new Scaled();
if (s.base !== 5) fail();
if (s.doubled !== 10) fail();
if (s.outerScale !== 30) fail();
