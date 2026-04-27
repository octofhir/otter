/* otter-test:
name = "json: parse rejects unterminated string"
[expect]
exit_code = 1
*/
JSON.parse("\"abc");
