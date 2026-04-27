/* otter-test:
name = "Math.PI / Math.E load through MathLoad"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const pi = Math.PI;
if (pi < 3.14 || pi > 3.142) fail();
const e = Math.E;
if (e < 2.71 || e > 2.72) fail();
