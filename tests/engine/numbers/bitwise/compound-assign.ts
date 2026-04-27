/* otter-test:
name = "bitwise: compound assignment operators write back"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let n = 12;
n &= 10;
if (n !== 8) fail();
n |= 1;
if (n !== 9) fail();
n ^= 0b1111;
if (n !== 6) fail();
n <<= 2;
if (n !== 24) fail();
n >>= 1;
if (n !== 12) fail();
n >>>= 0;
if (n !== 12) fail();
// Compound arithmetic on the same path.
let total = 5;
total += 2;
total *= 3;
total -= 1;
total /= 2;
if (total !== 10) fail();
// `**=` exercises the new Pow opcode.
let pow = 2;
pow **= 5;
if (pow !== 32) fail();
