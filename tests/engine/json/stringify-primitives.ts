/* otter-test:
name = "json: stringify primitives"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if (JSON.stringify(null) !== "null") fail();
if (JSON.stringify(true) !== "true") fail();
if (JSON.stringify(false) !== "false") fail();
if (JSON.stringify(42) !== "42") fail();
if (JSON.stringify(-3.5) !== "-3.5") fail();
// undefined → no output (returns undefined).
if (JSON.stringify(undefined) !== undefined) fail();
// Strings escape control + special characters.
if (JSON.stringify("a\nb\\c\"d") !== "\"a\\nb\\\\c\\\"d\"") fail();
