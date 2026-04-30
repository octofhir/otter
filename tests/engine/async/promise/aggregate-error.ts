/* otter-test:
name = "errors: AggregateError constructor + instanceof + errors / message own props"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// §20.5.7.1 `new AggregateError(errors, message?)`.
const a = new AggregateError([1, 2, 3], "many failures");
if (!(a instanceof AggregateError)) fail();
if (!(a instanceof Error)) fail();
if (a.name !== "AggregateError") fail();
if (a.message !== "many failures") fail();
if (!Array.isArray(a.errors)) fail();
if (a.errors.length !== 3) fail();
if (a.errors[0] !== 1) fail();
if (a.errors[2] !== 3) fail();

// Message argument is optional (defaults to inherited "").
const b = new AggregateError([]);
if (b.errors.length !== 0) fail();
if (b.message !== "") fail();
