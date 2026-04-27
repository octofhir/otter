/* otter-test:
name = "params: mixed defaults, destructuring, and rest in one signature"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function summarise({ kind = "u" }, count = 1, ...extras) {
    // Returns count + extras.length so caller can assert numerically
    // without paying the foundation's missing string-coerce path.
    if (kind === "u") return count + extras.length;
    if (kind === "ok") return count + extras.length + 100;
    return count + extras.length + 1000;
}
if (summarise({}) !== 1) fail();
if (summarise({ kind: "ok" }, 3, "a", "b") !== 105) fail();
if (summarise({ kind: "z" }, undefined, "x") !== 1002) fail();
