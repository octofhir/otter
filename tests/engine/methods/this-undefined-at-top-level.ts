/* otter-test:
name = "methods: top-level `this` is undefined"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
// Module top-level `this` follows the foundation strict default.
const t = this;
if (t !== undefined) fail();
// Free function calls also bind `this = undefined`.
function readThis() {
    return this;
}
if (readThis() !== undefined) fail();
