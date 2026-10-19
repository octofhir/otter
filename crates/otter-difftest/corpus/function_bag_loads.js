// Named loads of a function's own data properties: static constants on a
// constructor, on a native (Number), a bag that changes under optimized
// code, a prototype override, and a receiver that stops being a function.
function Direction() {}
Direction.FORWARD = 1;
Direction.BACKWARD = 2;
function Strength(v) { this.v = v; }
Strength.REQUIRED = new Strength(0);
function read(f) { return f.FORWARD + f.BACKWARD; }
function readNative() { return Number.MAX_SAFE_INTEGER > 0 ? 1 : 0; }
var total = 0;
for (var i = 0; i < 300000; i++) {
  total += read(Direction) + readNative() + Strength.REQUIRED.v;
  if (i === 200000) Direction.FORWARD = 10;
  if (i === 250000) Direction.extra = 5;
}
var other = { FORWARD: 100, BACKWARD: 200 };
total += read(other);
function Proxyish() {}
Proxyish.FORWARD = 3;
Proxyish.BACKWARD = 4;
total += read(Proxyish);
Object.setPrototypeOf(Proxyish, { FORWARD: 7, BACKWARD: 8 });
total += read(Proxyish);
delete Proxyish.FORWARD;
total += read(Proxyish);
console.log(total);
