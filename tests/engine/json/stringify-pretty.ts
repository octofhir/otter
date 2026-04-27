/* otter-test:
name = "json: stringify with space param indents output"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const expected = "{\n  \"a\": 1,\n  \"b\": [\n    2,\n    3\n  ]\n}";
if (JSON.stringify({ a: 1, b: [2, 3] }, null, 2) !== expected) fail();
// String space — first 10 bytes of the indent string.
const expectedStr = "{\n>>\"a\": 1\n}";
if (JSON.stringify({ a: 1 }, null, ">>") !== expectedStr) fail();
