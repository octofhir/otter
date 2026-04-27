/* otter-test:
name = "regexp: g flag tracks lastIndex through repeated .exec"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const re = /a/g;
if (re.lastIndex !== 0) fail();

const m1 = re.exec("abab");
if (m1 === null) fail();
if (m1[0] !== "a") fail();
if (re.lastIndex !== 1) fail();

const m2 = re.exec("abab");
if (m2 === null) fail();
if (re.lastIndex !== 3) fail();

// Third call → no more matches; lastIndex resets to 0.
if (re.exec("abab") !== null) fail();
if (re.lastIndex !== 0) fail();
