/* otter-test:
name = "abstract-ops: Array.isArray on every value shape"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Array literals + bracket-element shapes are arrays. The
// `new Array(...)` constructor form lands under task 73 (Array
// completion); the literal-only check captures the §7.2.2 path
// today.
if (!Array.isArray([])) fail();
if (!Array.isArray([1, 2, 3])) fail();
const aliased = [4, 5];
if (!Array.isArray(aliased)) fail();

// Non-array shapes — every primitive and every other heap shape.
if (Array.isArray(undefined)) fail();
if (Array.isArray(null)) fail();
if (Array.isArray(0)) fail();
if (Array.isArray(NaN)) fail();
if (Array.isArray("hi")) fail();
if (Array.isArray(true)) fail();
if (Array.isArray({})) fail();
if (Array.isArray({ length: 0 })) fail();
if (Array.isArray(function () {})) fail();
if (Array.isArray(() => {})) fail();
if (Array.isArray(new Map())) fail();
if (Array.isArray(new Set())) fail();
