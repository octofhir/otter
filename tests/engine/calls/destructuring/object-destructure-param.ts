/* otter-test:
name = "params: object destructuring with defaults and renames"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function add({ x, y = 9 }) {
    return x + y;
}
if (add({ x: 1 }) !== 10) fail();
if (add({ x: 1, y: 2 }) !== 3) fail();
// Renamed binding: `{ x: alias }` reads `x` and binds `alias`.
function origin({ x: a, y: b = 100 }) {
    return a + b;
}
if (origin({ x: 7, y: 8 }) !== 15) fail();
if (origin({ x: 7 }) !== 107) fail();
