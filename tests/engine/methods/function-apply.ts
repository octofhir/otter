/* otter-test:
name = "methods: Function.prototype.apply unpacks literal array args"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function readV() {
    return this.v;
}
const nine = readV.apply({ v: 9 }, []);
if (nine !== 9) fail();
const sum = function (a, b, c) {
    return this.base + a + b + c;
}.apply({ base: 100 }, [1, 2, 3]);
if (sum !== 106) fail();
