/* otter-test:
name = "Math.* core functions return the expected values"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if (Math.abs(-7) !== 7) fail();
if (Math.min(1, 2, 3) !== 1) fail();
if (Math.max(1, 2, 3) !== 3) fail();
if (Math.floor(3.9) !== 3) fail();
if (Math.ceil(3.1) !== 4) fail();
if (Math.round(2.5) !== 3) fail();
if (Math.trunc(-2.7) !== -2) fail();
if (Math.sqrt(16) !== 4) fail();
if (Math.pow(2, 10) !== 1024) fail();
// `**` is the spelling-equivalent of Math.pow.
if (2 ** 10 !== 1024) fail();
