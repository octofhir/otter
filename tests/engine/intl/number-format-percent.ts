/* otter-test:
name = "intl: NumberFormat percent style multiplies by 100"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const nf = new Intl.NumberFormat("en-US", { style: "percent" });
if (nf.format(0.25) !== "25%") fail();
if (nf.format(1) !== "100%") fail();
