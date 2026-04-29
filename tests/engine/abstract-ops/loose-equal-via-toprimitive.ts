/* otter-test:
name = "abstract-ops: == consults [Symbol.toPrimitive] / valueOf"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// `[Symbol.toPrimitive]` is called with hint `"default"` from
// loose equality (ECMA-262 §7.2.13 step 6 + §7.1.1).
const obj = {};
obj[Symbol.toPrimitive] = function (hint) {
    if (hint !== "default") fail();
    return 42;
};
if (!(obj == 42)) fail();
if (!(42 == obj)) fail();
if (obj == 7) fail();

// Falls through to valueOf on objects without `[Symbol.toPrimitive]`.
const numLike = {};
numLike.valueOf = function () {
    return 100;
};
if (!(numLike == 100)) fail();
if (numLike == 99) fail();

// Falls through to toString when valueOf returns an object.
const stringLike = {};
stringLike.valueOf = function () {
    return {};
};
stringLike.toString = function () {
    return "5";
};
if (!(stringLike == 5)) fail();
if (!(stringLike == "5")) fail();
