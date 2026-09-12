// Method lookup is committed before evaluating this and before any forwarded
// argument-object allocation or generated-frame materialization.
let reads = 0;
function throwingTarget() {}
Object.defineProperty(throwingTarget, "apply", {
  get() { reads++; throw new Error("lookup"); }
});
class Base {}
class Derived extends Base {
  constructor() { throwingTarget.apply(this, arguments); }
}
let message = "";
try { new Derived(); } catch (error) { message = error.message; }
if (reads !== 1 || message !== "lookup") throw new Error("lookup order");
reads = 0;
let calls = 0;
function target() {}
Object.defineProperty(target, "apply", {
  get() {
    reads++;
    return function(receiver, args) { calls++; return args[0]; };
  }
});
function forward(value) { return target.apply(null, arguments); }
let sum = 0;
for (let i = 0; i < 6000; i++) sum += forward(i);
if (reads !== 6000 || calls !== 6000 || sum !== 17997000) {
  throw new Error("forwarded lookup repeated or arguments corrupted");
}
console.log(reads + "|" + calls + "|" + sum);
