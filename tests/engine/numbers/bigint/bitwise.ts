/* otter-test:
name = "bigint: bitwise operators work without ToInt32 truncation"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// BigInt skips the i32-mod step that Number bitwise ops apply,
// so high-bit values survive.
const all = 0xffffffffffffffffn;
if ((all & 0xffffffffn) !== 0xffffffffn) fail();
if ((all | 0n) !== all) fail();
if ((0xff00n ^ 0x0ffn) !== 0xffffn) fail();
if ((1n << 65n) !== 36893488147419103232n) fail();
if (~0n !== -1n) fail();
