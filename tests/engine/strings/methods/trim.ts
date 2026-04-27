/* otter-test:
name = "string method: .trim / .trimStart / .trimEnd"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("  hi ".trim() !== "hi") fail();
if ("  hi ".trimStart() !== "hi ") fail();
if ("  hi ".trimEnd() !== "  hi") fail();
// Tabs and newlines are whitespace.
if ("\t\nhi\r\n".trim() !== "hi") fail();
// All whitespace collapses to empty.
if ("   ".trim() !== "") fail();
// No-op when already clean.
if ("hi".trim() !== "hi") fail();
