/* otter-test:
name = "intl: ListFormat conjunction / disjunction / unit"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

const conj = new Intl.ListFormat("en", { type: "conjunction" });
if (conj.format(["a"]) !== "a") fail();
if (conj.format(["a", "b"]) !== "a and b") fail();
if (conj.format(["a", "b", "c"]) !== "a, b, and c") fail();

const dis = new Intl.ListFormat("en", { type: "disjunction" });
if (dis.format(["x", "y"]) !== "x or y") fail();
if (dis.format(["x", "y", "z"]) !== "x, y, or z") fail();

const unit = new Intl.ListFormat("en", { type: "unit" });
if (unit.format(["1", "2"]) !== "1, 2") fail();
if (unit.format(["1", "2", "3"]) !== "1, 2, 3") fail();

// formatToParts returns an array of literal parts.
const parts = conj.formatToParts(["a", "b"]);
if (parts.length < 1) fail();
if (parts[0].type !== "literal") fail();

const opts = conj.resolvedOptions();
if (opts.type !== "conjunction") fail();
