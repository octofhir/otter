/* otter-test:
name = "json: parse handles \\n / \\t / \\\" / \\uXXXX"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const s = JSON.parse("\"a\\nb\\\\c\\\"d\\u0041\"");
if (s !== "a\nb\\c\"dA") fail();
