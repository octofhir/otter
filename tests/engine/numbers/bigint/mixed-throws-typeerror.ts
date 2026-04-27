/* otter-test:
name = "bigint: mixing Number and BigInt arithmetic throws"
[expect]
exit_code = 1
*/
// Mixed Number + BigInt is a spec TypeError. The runtime
// surfaces this as a TYPE_MISMATCH; the script exits non-zero.
const _ = 1n + 1;
