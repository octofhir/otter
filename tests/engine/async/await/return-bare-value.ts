/* otter-test:
name = "async: function returns a value, .then receives it"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
async function f() {
    return 7;
}
f().then((v) => {
    if (v !== 7) fail();
});
