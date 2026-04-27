/* otter-test:
name = "string method: .padStart / .padEnd"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
if ("42".padStart(5, "0") !== "00042") fail();
if ("42".padEnd(5, "0") !== "42000") fail();
// Default pad is a single space.
if ("ab".padStart(5) !== "   ab") fail();
if ("ab".padEnd(5) !== "ab   ") fail();
// Multi-char pad gets truncated to fit.
if ("x".padStart(5, "ab") !== "ababx") fail();
if ("x".padEnd(5, "ab") !== "xabab") fail();
// Already long enough → original.
if ("hello".padStart(3, "0") !== "hello") fail();
if ("hello".padEnd(3, "0") !== "hello") fail();
