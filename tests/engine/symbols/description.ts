/* otter-test:
name = "symbols: Symbol.prototype.description exposes the constructor argument"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const s = Symbol("hello");
if (s.description !== "hello") fail();
const empty = Symbol();
if (empty.description !== undefined) fail();
