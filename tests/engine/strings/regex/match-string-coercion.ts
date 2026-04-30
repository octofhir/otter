/* otter-test:
name = "string method: .match / .matchAll / .search coerce string args via RegExpCreate"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §22.1.3.13 — string arg becomes a non-global regex pattern.
const m = "hello world".match("o");
if (m === null) fail();
if (m[0] !== "o") fail();
if (m.index !== 4) fail();

// Pattern metacharacters honoured (it's a real regex compile).
const m2 = "abc123def".match("[0-9]+");
if (m2 === null || m2[0] !== "123") fail();
if (m2.index !== 3) fail();

// Non-match → null (not undefined).
if ("abc".match("z") !== null) fail();

// §22.1.3.14 — matchAll synthesises a `g`-flagged regex.
const all = [..."ababa".matchAll("a")];
if (all.length !== 3) fail();
if (all[0][0] !== "a" || all[1][0] !== "a" || all[2][0] !== "a") fail();
if (all[0].index !== 0 || all[1].index !== 2 || all[2].index !== 4) fail();

// §22.1.3.15 — search coerces to non-global regex.
if ("abc123".search("[0-9]") !== 3) fail();
if ("abc".search("z") !== -1) fail();

// Real RegExp args still work (no regression).
if ("foo".match(/o/)[0] !== "o") fail();
if ("aaa".search(/a/) !== 0) fail();
const ag = [..."abab".matchAll(/a/g)];
if (ag.length !== 2) fail();

// matchAll on a non-global RegExp arg is still a TypeError.
let threw = false;
try {
    "abc".matchAll(/a/);
} catch (_) {
    threw = true;
}
if (!threw) fail();
