/* otter-test:
name = "strings: encodeURI / encodeURIComponent / decode round-trip"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// encodeURI keeps reserved chars (`/`, `?`, `:`, …) intact.
if (encodeURI("https://example.com/a b?x=1") !== "https://example.com/a%20b?x=1") fail();
// encodeURIComponent escapes them.
if (encodeURIComponent("a/b?c") !== "a%2Fb%3Fc") fail();
// decodeURIComponent inverse.
if (decodeURIComponent("hello%20world%21") !== "hello world!") fail();
// decodeURI inverse.
if (decodeURI("https://example.com/a%20b") !== "https://example.com/a b") fail();
// Unicode round-trip.
let mark = "café";
if (decodeURIComponent(encodeURIComponent(mark)) !== mark) fail();
