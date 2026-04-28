/* otter-test:
name = "symbols: +obj consults [Symbol.toPrimitive]"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const obj = {};
obj[Symbol.toPrimitive] = function (hint) {
    return 42;
};
const n = +obj;
if (n !== 42) fail();
