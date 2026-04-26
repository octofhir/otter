/* otter-test:
name = "object: string-quoted keys at literal site"
[expect]
exit_code = 0
*/
// Foundation slice supports static-identifier and string-literal
// keys at the literal site; computed-key reads (`o["x"]`) wait
// for a follow-up.
let o = { "first": "Ada", "second": 7 };
o.first;
o.second;
