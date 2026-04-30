/* otter-test:
name = "intl: DisplayNames maps codes to English display names"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Languages.
const lang = new Intl.DisplayNames("en", { type: "language" });
if (lang.of("en") !== "English") fail();
if (lang.of("fr") !== "French") fail();
if (lang.of("zh") !== "Chinese") fail();

// Regions.
const region = new Intl.DisplayNames("en", { type: "region" });
if (region.of("US") !== "United States") fail();
if (region.of("FR") !== "France") fail();

// Currencies.
const cur = new Intl.DisplayNames("en", { type: "currency" });
if (cur.of("USD") !== "US Dollar") fail();
if (cur.of("EUR") !== "Euro") fail();

// Fallback: code returns the code; none returns undefined.
const fallbackCode = new Intl.DisplayNames("en", {
    type: "language",
    fallback: "code",
});
if (fallbackCode.of("zz") !== "zz") fail();
const fallbackNone = new Intl.DisplayNames("en", {
    type: "language",
    fallback: "none",
});
if (fallbackNone.of("zz") !== undefined) fail();

const opts = lang.resolvedOptions();
if (opts.type !== "language") fail();
