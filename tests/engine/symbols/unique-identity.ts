/* otter-test:
name = "symbols: each Symbol() call produces a fresh identity"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const a = Symbol("k");
const b = Symbol("k");
if (a === b) fail();
const c = a;
if (c !== a) fail();
