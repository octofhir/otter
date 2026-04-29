/* otter-test:
name = "abstract-ops: OrdinaryToPrimitive falls through to toString"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// valueOf returns an object — OrdinaryToPrimitive must fall
// through to toString per §7.1.1.1 step 5.
const obj = {};
obj.valueOf = function () {
    return {};
};
obj.toString = function () {
    return "fallback";
};

const concat = obj + "!";
if (concat !== "fallback!") fail();

// Object with only toString defined — `valueOf` is missing,
// OrdinaryToPrimitive skips to `toString` directly.
const onlyToString = {};
onlyToString.toString = function () {
    return "only-to-string";
};
const out = onlyToString + " here";
if (out !== "only-to-string here") fail();
