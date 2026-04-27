/* otter-test:
name = "Number.prototype.toString(radix) renders integers"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ((255).toString(16) !== "ff") fail();
if ((255).toString() !== "255") fail();
if ((255).toString(2) !== "11111111") fail();
if ((-1).toString(16) !== "-1") fail();
