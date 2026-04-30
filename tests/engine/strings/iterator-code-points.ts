/* otter-test:
name = "strings: for...of yields code points (combines surrogate pairs)"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// 😀 = U+1F600 (surrogate pair D83D DE00); "ab😀c" is 5 UTF-16 units
// but should yield 4 code-point steps under §22.1.5.
const s = "ab😀c";
if (s.length !== 5) fail();

const out = [];
for (const ch of s) {
    out.push(ch);
}
if (out.length !== 4) fail();
if (out[0] !== "a") fail();
if (out[1] !== "b") fail();
if (out[2] !== "😀") fail();
if (out[2].length !== 2) fail();
if (out[3] !== "c") fail();

// Spread also rides StringIterator.
const arr = [..."a😀z"];
if (arr.length !== 3) fail();
if (arr[0] !== "a") fail();
if (arr[1] !== "😀") fail();
if (arr[2] !== "z") fail();

// Adjacent supplementary code points combine independently.
const two = "😀😀";
const tt = [];
for (const ch of two) {
    tt.push(ch);
}
if (tt.length !== 2) fail();
if (tt[0] !== "😀") fail();
if (tt[1] !== "😀") fail();

// ASCII-only iteration unchanged.
const ascii = [..."hi"];
if (ascii.length !== 2) fail();
if (ascii[0] !== "h" || ascii[1] !== "i") fail();

// Lone surrogate is yielded as a single 1-unit string.
const lone = "\uD800";
const lo = [];
for (const ch of lone) {
    lo.push(ch);
}
if (lo.length !== 1) fail();
if (lo[0] !== "\uD800") fail();
if (lone.length !== 1) fail();

// High surrogate followed by non-low: high yielded alone.
const broken = "\uD83Dx";
const bo = [];
for (const ch of broken) {
    bo.push(ch);
}
if (bo.length !== 2) fail();
if (bo[0] !== "\uD83D") fail();
if (bo[1] !== "x") fail();
