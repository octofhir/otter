/* otter-test:
name = "calls: Function.prototype .name / .length / .toString"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function add(a: number, b: number): number {
  return a + b;
}
if (add.name !== "add") fail();
if (add.length !== 2) fail();

// Arrow function — name is the binding name when assigned, length
// is its declared param count.
let arrow = (x: number, y: number) => x + y;
if (arrow.length !== 2) fail();

// Rest parameter excluded from `length`.
function variadic(a: number, b: number, ...rest: number[]): number {
  return a + b + rest.length;
}
if (variadic.length !== 2) fail();

// Bound function: name prefix + length minus bound args.
let bound = add.bind(null, 5);
if (bound.name !== "bound add") fail();
if (bound.length !== 1) fail();
if (bound(7) !== 12) fail();

// Class constructor.
class Animal {
  static count = 0;
  constructor(public name: string) {}
}
if (Animal.name !== "Animal") fail();
if (Animal.length !== 1) fail();

// toString — foundation returns a placeholder containing the name.
let s = add.toString();
if (typeof s !== "string") fail();
if (s.indexOf("add") < 0) fail();
if (Animal.toString().indexOf("Animal") < 0) fail();
