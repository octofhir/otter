/* otter-test:
name = "methods: arrow inherits enclosing this lexically"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
function outer() {
    return () => this;
}
const got = outer.call({ tag: "outer" });
const here = got();
if (here.tag !== "outer") fail();
// Arrow ignores explicit .call receiver — lexical wins.
const stillOuter = got.call({ tag: "other" });
if (stillOuter.tag !== "outer") fail();
// Top-level arrow captures `this = undefined`.
const top = () => this;
if (top() !== undefined) fail();
