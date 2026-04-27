/* otter-test:
name = "bigint: strict equality across Number / BigInt is always false"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// `===` is *kind-sensitive*: 1n and 1 are not strictly equal.
if (1n === 1) fail();
if (0n === 0) fail();
// Two BigInts with the same value are strictly equal even if
// allocated separately.
if (10n !== 10n) fail();
const a = 999999999999999999999999n;
const b = 999999999999999999999999n;
if (a !== b) fail();
