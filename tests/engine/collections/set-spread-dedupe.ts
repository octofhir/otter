/* otter-test:
name = "collections: [...new Set(arr)] dedupes"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const arr = [...new Set([1, 1, 2, 3, 3, 3])];
if (arr.length !== 3) fail();
if (arr[0] !== 1) fail();
if (arr[1] !== 2) fail();
if (arr[2] !== 3) fail();
