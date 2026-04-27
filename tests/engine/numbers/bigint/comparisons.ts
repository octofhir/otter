/* otter-test:
name = "bigint: comparison operators across BigInt and Number"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if (!(1n < 2n)) fail();
if (!(2n > 1n)) fail();
if (!(2n >= 2n)) fail();
if (!(2n <= 2n)) fail();
// Mixed Number / BigInt comparisons are allowed (only equality
// is strictly kind-separated). 5n vs 5 compares equal-by-value.
if (!(5n <= 5)) fail();
if (5n < 5) fail();
if (!(5n < 6)) fail();
// 5n vs 4.5 — BigInt is greater because the integer part wins.
if (!(5n > 4.5)) fail();
