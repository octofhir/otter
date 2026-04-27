/* otter-test:
name = "params: default value applies when arg is undefined"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function add(a, b = 5) {
    return a + b;
}
if (add(1) !== 6) fail();
if (add(1, 2) !== 3) fail();
// Explicitly passing `undefined` triggers the default.
if (add(1, undefined) !== 6) fail();
// Defaults can reference earlier params.
function pair(a, b = a + 1) {
    return a + b;
}
if (pair(10) !== 21) fail();
if (pair(10, 100) !== 110) fail();
