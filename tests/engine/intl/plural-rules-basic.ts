/* otter-test:
name = "intl: PluralRules cardinal + ordinal English categories"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}

// Cardinal — `one` for 1, `other` otherwise.
const pr = new Intl.PluralRules("en");
if (pr.select(0) !== "other") fail();
if (pr.select(1) !== "one") fail();
if (pr.select(2) !== "other") fail();
if (pr.select(100) !== "other") fail();

// Ordinal — English suffix categories per CLDR.
const ord = new Intl.PluralRules("en", { type: "ordinal" });
if (ord.select(1) !== "one") fail();   // 1st
if (ord.select(2) !== "two") fail();   // 2nd
if (ord.select(3) !== "few") fail();   // 3rd
if (ord.select(4) !== "other") fail(); // 4th
if (ord.select(11) !== "other") fail(); // 11th
if (ord.select(21) !== "one") fail();  // 21st

const opts = pr.resolvedOptions();
if (opts.type !== "cardinal") fail();
// Locale fallback returns the supplied tag verbatim (foundation
// skips full BCP-47 canonicalisation).
if (opts.locale !== "en") fail();
