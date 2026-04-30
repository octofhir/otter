/* otter-test:
name = "string: String(value) constructor + fromCharCode / fromCodePoint"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
if (String(42) !== "42") fail();
if (String(true) !== "true") fail();
if (String(null) !== "null") fail();
if (String(undefined) !== "undefined") fail();
if (String() !== "") fail();
if (String("hi") !== "hi") fail();

if (String.fromCharCode(72, 101, 108, 108, 111) !== "Hello") fail();
if (String.fromCharCode(65) !== "A") fail();
if (String.fromCodePoint(72, 105) !== "Hi") fail();
// 0x1F600 (😀) is a surrogate-pair code point — UTF-16 length is 2.
let smiley = String.fromCodePoint(0x1F600);
if (smiley.length !== 2) fail();
