/* otter-test:
name = "intl: DateTimeFormat formats Temporal.PlainDate"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const pd = Temporal.PlainDate.from("2024-12-31");
const dtf = new Intl.DateTimeFormat("en-US");
const out = dtf.format(pd);
if (out !== "12/31/2024") fail();
