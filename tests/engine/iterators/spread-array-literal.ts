/* otter-test:
name = "iterators: spread expands array literals"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const inner = [2, 3];
const merged = [1, ...inner, 4];
if (merged.length !== 4) fail();
if (merged[0] !== 1) fail();
if (merged[1] !== 2) fail();
if (merged[2] !== 3) fail();
if (merged[3] !== 4) fail();
// Spreading a string yields its code units.
const letters = [..."hi"];
if (letters.length !== 2) fail();
if (letters[0] !== "h") fail();
if (letters[1] !== "i") fail();
