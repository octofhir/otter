/* otter-test:
name = "regexp: lookbehind, \\p{}, and v-flag set notation"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Lookbehind (positive + negative).
const lookBehind = /(?<=foo)bar/.exec("foobar");
if (lookBehind === null || lookBehind[0] !== "bar") fail();
const negLookBehind = /(?<!foo)bar/.exec("zzbar");
if (negLookBehind === null || negLookBehind[0] !== "bar") fail();
if (/(?<!foo)bar/.exec("foobar") !== null) fail();

// Unicode property escapes (require `u`).
const letter = /\p{Letter}+/u.exec("123 abcüd 456");
if (letter === null) fail();
if (letter[0] !== "abcüd") fail();
const decimal = /\p{Nd}+/u.exec("xx 42 yy");
if (decimal === null || decimal[0] !== "42") fail();

// `v` flag — set difference.
const setDiff = /[\p{ASCII_Hex_Digit}--[0-9]]+/v.exec("123abcDEF456");
if (setDiff === null) fail();
if (setDiff[0] !== "abcDEF") fail();
if (/abc/v.flags !== "v") fail();
if (/abc/v.unicodeSets !== true) fail();
if (/abc/u.unicodeSets !== false) fail();
