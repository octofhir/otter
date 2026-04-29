/* otter-test:
name = "temporal: PlainDate.from + add({ days: 1 }) crosses year boundary"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const d = Temporal.PlainDate.from("2024-12-31");
if (d.year !== 2024) fail();
if (d.month !== 12) fail();
if (d.day !== 31) fail();
const next = d.add({ days: 1 });
if (next.toString() !== "2025-01-01") fail();
if (next.year !== 2025) fail();
if (next.month !== 1) fail();
if (next.day !== 1) fail();
const back = next.subtract({ days: 1 });
if (back.equals(d) !== true) fail();
