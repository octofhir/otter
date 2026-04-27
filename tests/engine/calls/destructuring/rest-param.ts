/* otter-test:
name = "params: rest collects trailing args into a fresh array"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function count(...rest) {
    return rest.length;
}
if (count() !== 0) fail();
if (count(1, 2, 3) !== 3) fail();
function leadingTrailing(first, ...rest) {
    return rest.length;
}
if (leadingTrailing("x") !== 0) fail();
if (leadingTrailing("x", 1, 2, 3) !== 3) fail();
// Rest array iterates like a regular array.
function sum(...nums) {
    let total = 0;
    for (let n of nums) {
        total = total + n;
    }
    return total;
}
if (sum(1, 2, 3, 4) !== 10) fail();
