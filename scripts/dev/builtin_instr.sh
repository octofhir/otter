#!/usr/bin/env bash
# Retired instructions per builtin call: each case runs N calls inside a
# hot function; the same loop around a plain comparison is subtracted.
# usage: scripts/dev/builtin_instr.sh <binary> [N] [case...]
O=${1:?binary}; N=${2:-20000000}; shift 2 2>/dev/null
D=$(mktemp -d)
emit() { # name, setup, expression over i
  cat >"$D/$1.js" <<JS
$2
let s = 0;
const f = (i) => ($3) ? 1 : 2;
for (let i = 0; i < $N; i++) s += f(i);
console.log(s);
JS
}
STR="const str = 'hello world, this is a flat string'; const arr = [1,2,3,4,5,6,7,8]; const obj = {a:1,b:2,c:3}; const objs = [obj, [1]];"
emit empty "$STR" "i === 3"
emit object_is "$STR" "Object.is(i, 3)"
emit is_integer "$STR" "Number.isInteger(i)"
emit is_safe_integer "$STR" "Number.isSafeInteger(i)"
emit is_nan "$STR" "Number.isNaN(i)"
emit is_finite "$STR" "Number.isFinite(i)"
emit is_array "$STR" "Array.isArray(objs[i & 1])"
emit get_prototype_of "$STR" "Object.getPrototypeOf(obj) === null"
emit char_at "$STR" "str.charAt(i & 15) === 'h'"
emit char_code_at "$STR" "str.charCodeAt(i & 15) === 104"
emit code_point_at "$STR" "str.codePointAt(i & 15) === 104"
emit index_of "$STR" "str.indexOf('this') === 3"
emit includes "$STR" "str.includes('flat')"
emit starts_with "$STR" "str.startsWith('hello')"
emit ends_with "$STR" "str.endsWith('string')"
emit slice "$STR" "str.slice(i & 7, 12).length === 3"
emit substring "$STR" "str.substring(i & 7, 12).length === 3"
emit from_char_code "$STR" "String.fromCharCode(97 + (i & 7)) === 'a'"
emit array_index_of "$STR" "arr.indexOf(i & 7) === 2"
emit array_includes "$STR" "arr.includes(i & 15)"
emit math_max3 "$STR" "Math.max(i, 5, 9) === 9"
emit math_min3 "$STR" "Math.min(i, 5, 9) === 0"
emit object_keys "$STR" "Object.keys(obj).length === 3"
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
base=$(instr "$O" "$D/empty.js")
cases=${*:-object_is is_integer is_safe_integer is_nan is_finite is_array get_prototype_of char_at char_code_at code_point_at index_of includes starts_with ends_with slice substring from_char_code array_index_of array_includes math_max3 math_min3 object_keys}
for c in $cases; do
  n=$(instr "$O" "$D/$c.js")
  printf '%-18s %6d instr/call\n' "$c" $(( (n - base) / N ))
done
rm -rf "$D"
