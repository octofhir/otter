/* otter-test:
name = "promise: Promise.race settles with the first fulfilled value"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
Promise.race([Promise.resolve("a"), Promise.resolve("b")]).then((v) => {
    if (v !== "a") fail();
});
