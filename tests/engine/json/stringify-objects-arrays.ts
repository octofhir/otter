/* otter-test:
name = "json: stringify objects + arrays preserve insertion order"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Insertion order survives.
if (JSON.stringify({ b: 1, a: 2 }) !== "{\"b\":1,\"a\":2}") fail();
// Nested arrays + objects.
if (JSON.stringify({ x: [1, 2, 3] }) !== "{\"x\":[1,2,3]}") fail();
// Empty containers.
if (JSON.stringify({}) !== "{}") fail();
if (JSON.stringify([]) !== "[]") fail();
// Mixed value types.
if (JSON.stringify([1, "two", true, null]) !== "[1,\"two\",true,null]") fail();
