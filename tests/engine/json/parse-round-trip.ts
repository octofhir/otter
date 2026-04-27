/* otter-test:
name = "json: parse round-trip on object + array"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const obj = JSON.parse("{\"x\":[1,2,3]}");
if (obj.x[0] !== 1) fail();
if (obj.x[1] !== 2) fail();
if (obj.x[2] !== 3) fail();

// Nested objects.
const nested = JSON.parse("{\"a\":{\"b\":{\"c\":42}}}");
if (nested.a.b.c !== 42) fail();

// Heterogeneous array.
const arr = JSON.parse("[true, null, 1.5, \"hi\"]");
if (arr[0] !== true) fail();
if (arr[1] !== null) fail();
if (arr[2] !== 1.5) fail();
if (arr[3] !== "hi") fail();
