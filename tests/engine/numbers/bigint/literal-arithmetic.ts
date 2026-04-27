/* otter-test:
name = "bigint: literal arithmetic preserves precision past 2^53"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const big = 9007199254740993n;
if (big + 1n !== 9007199254740994n) fail();
if (big - 1n !== 9007199254740992n) fail();
if (2n * 1000000000000n !== 2000000000000n) fail();
if (1000n / 7n !== 142n) fail();
if (1000n % 7n !== 6n) fail();
if (-5n !== 0n - 5n) fail();
if (2n ** 64n !== 18446744073709551616n) fail();
