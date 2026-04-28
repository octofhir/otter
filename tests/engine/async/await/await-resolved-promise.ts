/* otter-test:
name = "async: await Promise.resolve unwraps and continues"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
async function f() {
    let x = await Promise.resolve(1);
    return x + 1;
}
f().then((v) => {
    if (v !== 2) fail();
});
