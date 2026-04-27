/* otter-test:
name = "iterators: spread fans args into call expressions"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function add3(a, b, c) {
    return a + b + c;
}
const trio = [1, 2, 3];
if (add3(...trio) !== 6) fail();
// Mixed leading + spread + trailing arguments compose left-to-right.
if (add3(10, ...[20, 30]) !== 60) fail();
// Method spread: `this` stays bound to the receiver.
const o = {
    base: 100,
    sum: function (a, b) {
        return this.base + a + b;
    },
};
if (o.sum(...[1, 2]) !== 103) fail();
