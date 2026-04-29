/* otter-test:
name = "temporal: PlainTime.from + components"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const t = Temporal.PlainTime.from("12:34:56");
if (t.hour !== 12) fail();
if (t.minute !== 34) fail();
if (t.second !== 56) fail();
const later = t.add({ minutes: 30 });
if (later.hour !== 13) fail();
if (later.minute !== 4) fail();
