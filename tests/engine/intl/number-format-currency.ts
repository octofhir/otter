/* otter-test:
name = "intl: NumberFormat currency style renders en-US USD"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const nf = new Intl.NumberFormat("en-US", { style: "currency", currency: "USD" });
const out = nf.format(1234.5);
if (out !== "$1,234.50") fail();
const opts = nf.resolvedOptions();
if (opts.style !== "currency") fail();
if (opts.currency !== "USD") fail();
