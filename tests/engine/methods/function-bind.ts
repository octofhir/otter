/* otter-test:
name = "methods: Function.prototype.bind freezes this and prefix args"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function readV() {
    return this.v;
}
const five = readV.bind({ v: 5 });
if (five() !== 5) fail();
// Bound `this` survives an explicit `.call`/`.apply` override.
if (five.call({ v: 99 }) !== 5) fail();
const adder = function (a, b, c) {
    return this.base + a + b + c;
}.bind({ base: 1000 }, 1, 2);
if (adder(3) !== 1006) fail();
// Re-bind: the outer bind's `this` wins (per ES spec), and bound
// argument lists chain with the outer prefix in front of the inner
// one. So `adder.bind({base:0}, 4)(5)` forwards `(1, 2, 4, 5)` to
// the underlying function with `this = {base: 1000}`.
const inner = adder.bind({ base: 0 }, 4);
if (inner(5) !== 1007) fail();
