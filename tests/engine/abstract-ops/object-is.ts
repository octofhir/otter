/* otter-test:
name = "abstract-ops: Object.is — SameValue semantics"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// NaN equals itself.
if (!Object.is(NaN, NaN)) fail();
if (!Object.is(0 / 0, 0 / 0)) fail();

// +0 and -0 are distinct under SameValue.
if (Object.is(0, -0)) fail();
if (Object.is(-0, 0)) fail();
if (!Object.is(-0, -0)) fail();
if (!Object.is(0, 0)) fail();

// Primitives match by value.
if (!Object.is(1, 1)) fail();
if (!Object.is("hi", "hi")) fail();
if (!Object.is(true, true)) fail();
if (!Object.is(null, null)) fail();
if (!Object.is(undefined, undefined)) fail();

// Cross-kind / cross-type pairs are not equal.
if (Object.is(0, "0")) fail();
if (Object.is(null, undefined)) fail();
if (Object.is(1, true)) fail();

// Object identity.
const o = {};
if (!Object.is(o, o)) fail();
if (Object.is({}, {})) fail();
const arr = [];
if (!Object.is(arr, arr)) fail();
if (Object.is([], [])) fail();
