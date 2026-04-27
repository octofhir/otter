/* otter-test:
name = "string method: .search(regex) returns index or -1"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("hello world".search(/world/) !== 6) fail();
if ("abc".search(/^a/) !== 0) fail();
if ("abc".search(/zz/) !== -1) fail();
// Empty pattern matches at start.
if ("abc".search(/(?:)/) !== 0) fail();
