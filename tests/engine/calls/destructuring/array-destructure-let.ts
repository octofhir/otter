/* otter-test:
name = "params: array destructuring in params and let bindings"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Param-position array destructuring with rest tail.
function head([first, second, ...tail]) {
    return first + second + tail.length;
}
if (head([1, 2, 3, 4, 5]) !== 6) fail();
// `let [...]` bindings.
const arr = [10, 20, 30];
const [a, b, c] = arr;
if (a !== 10) fail();
if (b !== 20) fail();
if (c !== 30) fail();
// Rest in let.
const [first, ...rest] = [1, 2, 3, 4];
if (first !== 1) fail();
if (rest.length !== 3) fail();
if (rest[2] !== 4) fail();
