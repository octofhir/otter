/* otter-test:
name = "async: await of a non-promise resolves with the value"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
async function f() {
    let x = await 42;
    return x;
}
f().then((v) => {
    if (v !== 42) fail();
});
