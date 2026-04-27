/* otter-test:
name = "json: parse rejects bare identifiers (NaN, Infinity, undefined)"
[expect]
exit_code = 1
*/
JSON.parse("NaN");
