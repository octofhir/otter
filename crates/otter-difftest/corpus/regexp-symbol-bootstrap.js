const re = /a+/g;
const input = "caaab";
const match = re[Symbol.match](input);
re.lastIndex = 0;
const search = re[Symbol.search](input);
const replaced = re[Symbol.replace](input, "x");
const split = /a/[Symbol.split]("baac");
const all = [...re[Symbol.matchAll](input)].map((entry) => entry[0]);

console.log(JSON.stringify({ match, search, replaced, split, all }));
