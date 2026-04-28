/* otter-test:
name = "modules: import.meta.url is the canonical file:// URL"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
let url = import.meta.url;
if (url.indexOf("file://") !== 0) fail();
if (url.indexOf("entry.ts") < 0) fail();
