/* otter-test:
name = "Number.prototype.toFixed(digits) formats with fixed precision"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ((3.14159).toFixed(2) !== "3.14") fail();
if ((1).toFixed(0) !== "1") fail();
if ((0.5).toFixed(3) !== "0.500") fail();
