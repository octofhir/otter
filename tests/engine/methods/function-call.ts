/* otter-test:
name = "methods: Function.prototype.call binds explicit this"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function readV() {
    return this.v;
}
const seven = readV.call({ v: 7 });
if (seven !== 7) fail();
const sum = function (a, b) {
    return this.base + a + b;
}.call({ base: 10 }, 1, 2);
if (sum !== 13) fail();
