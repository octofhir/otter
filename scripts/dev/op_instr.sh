#!/usr/bin/env bash
# Retired instructions per basic operation of otter, node and bun: each case
# runs its body N times inside one function, the bare loop is subtracted.
# usage: scripts/dev/op_instr.sh <binary> [N] [case...]
O=${1:?binary}; N=${2:-10000000}; shift 2 2>/dev/null || shift $#
D=benchmarks/results/op-instr; mkdir -p "$D"
BIG=$(for k in $(seq 1 60); do printf 'for (let j = 0; j < 100; j++) { t += j * %d; t ^= j; } ' $k; done)
cat >"$D/prelude.js" <<JS
function big(x) { if (x === -1) { let t = 0; $BIG return t; } return x + 1; }
function big2(a, b) { if (a === -1) { let t = 0; $BIG return t; } return a + b; }
function big0() { if (out === -1) { let t = 0; $BIG return t; } return 1; }
function P(x) { this.x = x; this.y = x + 1; this.z = null; }
P.prototype.get = function () { if (this.x === -1) { let t = 0; $BIG return t; } return this.x; };
class K { constructor(x) { this.x = x; this.y = 0; } m() { if (this.x === -1) { let t = 0; $BIG return t; } return this.y + 1; } }
const pobj = new P(3), kobj = new K(3);
const arr = []; for (let i = 0; i < 1024; i++) arr.push(i);
const oarr = []; for (let i = 0; i < 1024; i++) oarr.push(new P(i));
const keep = new Array(1024).fill(null);
const darr = new Float64Array(1024);
const str = 'abcdefghijklmnopqrstuvwxyz'.repeat(40);
const map = new Map(); for (let i = 0; i < 1024; i++) map.set(i, i);
const keys = []; for (let i = 0; i < 1024; i++) keys.push('k' + i);
const sset = new Set(keys.filter((_, i) => i % 2));
const dict = {}; for (const k of keys) dict[k] = 1;
const re = /k1[0-9]+/, re2 = /k(\d)(\d)/;
const smallarr = [1, 2, 3, 4];
const pair = [1, 2];
function argsf() { return arguments.length + arguments[0]; }
function restf(...xs) { return xs.length + xs[0]; }
const bound = big.bind(null);
const gobj = { get v() { return 3; } };
let out = 0;
JS
case_body() {
  case $1 in
    empty) echo 's = (s + 1) | 0;' ;;
    call1) echo 's = big(s) | 0;' ;;
    call2) echo 's = big2(s, 1) | 0;' ;;
    call0) echo 's = (s + big0()) | 0;' ;;
    method) echo 's = (s + pobj.get()) | 0;' ;;
    classm) echo 's = (s + kobj.m()) | 0;' ;;
    load) echo 's = (s + pobj.x) | 0;' ;;
    store) echo 's = (s + 1) | 0; pobj.y = s;' ;;
    storeobj) echo 's = (s + 1) | 0; pobj.z = oarr[i & 1023];' ;;
    aload) echo 's = (s + arr[i & 1023]) | 0;' ;;
    astore) echo 's = (s + 1) | 0; arr[i & 1023] = s;' ;;
    astoreobj) echo 's = (s + 1) | 0; keep[i & 1023] = oarr[i & 1023];' ;;
    oload) echo 's = (s + oarr[i & 1023].x) | 0;' ;;
    dstore) echo 's = (s + 1) | 0; darr[i & 1023] = s * 0.5;' ;;
    newobj) echo 's = (s + 1) | 0; keep[i & 1023] = new P(s);' ;;
    newclass) echo 's = (s + 1) | 0; keep[i & 1023] = new K(s);' ;;
    literal) echo 's = (s + 1) | 0; keep[i & 1023] = { a: s, b: 1 };' ;;
    arrlit) echo 's = (s + 1) | 0; keep[i & 1023] = [s, 1, 2];' ;;
    closure) echo 's = (s + 1) | 0; keep[i & 1023] = () => s;' ;;
    pushpop) echo 'arr.push(s); s = (s + arr.pop()) | 0;' ;;
    charcode) echo 's = (s + str.charCodeAt(i & 1023)) | 0;' ;;
    concat) echo 's = (s + 1) | 0; keep[i & 1023] = "k" + (i & 1023);' ;;
    closurecall) echo 's = (s + fns[i & 3](s)) | 0;' ;;
    mapget) echo 's = (s + map.get(i & 1023)) | 0;' ;;
    mapset) echo 's = (s + 1) | 0; map.set(i & 1023, s);' ;;
    setgetstr) echo 's = (s + (sset.has(keys[i & 1023]) ? 1 : 0)) | 0;' ;;
    objkey) echo 's = (s + dict[keys[i & 1023]]) | 0;' ;;
    objkeyset) echo 's = (s + 1) | 0; dict[keys[i & 1023]] = s;' ;;
    retest) echo 's = (s + (re.test(keys[i & 1023]) ? 1 : 0)) | 0;' ;;
    reexec) echo 's = (s + (re2.exec(keys[i & 1023]) !== null ? 1 : 0)) | 0;' ;;
    indexof) echo 's = (s + str.indexOf("xyz", i & 511)) | 0;' ;;
    slice) echo 's = (s + 1) | 0; keep[i & 1023] = str.slice(i & 511, (i & 511) + 8);' ;;
    streq) echo 's = (s + (keys[i & 1023] === keys[(i + 1) & 1023] ? 1 : 0)) | 0;' ;;
    substrcmp) echo 's = (s + (keys[i & 1023] < keys[(i + 7) & 1023] ? 1 : 0)) | 0;' ;;
    split) echo 's = (s + "a,b,c".split(",").length) | 0;' ;;
    join) echo 's = (s + smallarr.join(",").length) | 0;' ;;
    tostr) echo 's = (s + String(i & 1023).length) | 0;' ;;
    spread) echo 's = (s + 1) | 0; keep[i & 1023] = [...smallarr];' ;;
    destruct) echo 'const { x, y } = pobj; s = (s + x + y) | 0;' ;;
    forof) echo 'for (const v of smallarr) s = (s + v) | 0;' ;;
    forin) echo 'for (const k in pobj) s = (s + k.length) | 0;' ;;
    instanceof) echo 's = (s + (oarr[i & 1023] instanceof P ? 1 : 0)) | 0;' ;;
    typeofs) echo 's = (s + (typeof keep[i & 1023] === "object" ? 1 : 0)) | 0;' ;;
    trycatch) echo 'try { s = (s + 1) | 0; if (s === -5) throw 1; } catch (e) { s = 0; }' ;;
    args) echo 's = (s + argsf(s, 1, 2)) | 0;' ;;
    rest) echo 's = (s + restf(s, 1, 2)) | 0;' ;;
    applyc) echo 's = (s + big2.apply(null, pair)) | 0;' ;;
    callc) echo 's = (s + big2.call(null, s, 1)) | 0;' ;;
    bindc) echo 's = (s + bound(s)) | 0;' ;;
    getter) echo 's = (s + gobj.v) | 0;' ;;
    proto) echo 's = (s + oarr[i & 1023].get()) | 0;' ;;
    mathf) echo 's = (s + Math.floor(Math.sqrt(i & 1023))) | 0;' ;;
    dmath) echo 's = (s + ((darr[i & 1023] * 1.5 + 0.25) | 0)) | 0;' ;;
    strcat2) echo 's = (s + 1) | 0; keep[i & 1023] = keys[i & 1023] + keys[(i + 1) & 1023];' ;;
    template) echo 's = (s + 1) | 0; keep[i & 1023] = `a${i & 1023}b`;' ;;
    jsonp) echo 's = (s + JSON.parse("[1,2,3]").length) | 0;' ;;
    sortsmall) echo 's = (s + [3, 1, 2].sort((a, b) => a - b)[0]) | 0;' ;;
    arrmap) echo 's = (s + smallarr.map((v) => v + 1)[0]) | 0;' ;;
    arrfilter) echo 's = (s + smallarr.filter((v) => v > 1).length) | 0;' ;;
    arrindexof) echo 's = (s + smallarr.indexOf(3)) | 0;' ;;
    objkeys) echo 's = (s + Object.keys(pobj).length) | 0;' ;;
    switchs) echo 'switch (keys[i & 7]) { case "k0": s = (s + 1) | 0; break; case "k1": s = (s + 2) | 0; break; default: s = (s + 3) | 0; }' ;;
    *) echo "unknown case $1" >&2; exit 1 ;;
  esac
}
CASES=${*:-call0 call1 call2 method classm closurecall load store storeobj aload astore astoreobj oload dstore newobj newclass literal arrlit closure pushpop charcode concat}
for c in empty $CASES; do
  { cat "$D/prelude.js"
    echo "const fns = [0, 1, 2, 3].map((k) => (x) => { if (x === -1) { let t = 0; $BIG return t; } return x + k; });"
    echo "function f(n) { let s = 0; for (let i = 0; i < n; i++) { $(case_body $c) } out += s; }"
    echo "f(1000); f($N);"
    echo "console.log(out);"; } >"$D/$c.js"
done
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
printf '%-12s %7s %7s %7s %7s\n' case otter node bun o/node
bo=$(instr "$O" "$D/empty.js"); bn=$(instr node "$D/empty.js"); bb=$(instr bun "$D/empty.js")
for c in $CASES; do
  vals=($(( ($(instr "$O" "$D/$c.js") - bo) / N )) $(( ($(instr node "$D/$c.js") - bn) / N )) $(( ($(instr bun "$D/$c.js") - bb) / N )))
  r=$(python3 -c "print(f'{${vals[0]}/max(${vals[1]},1):.2f}')")
  printf '%-12s %7s %7s %7s %7s\n' $c ${vals[0]} ${vals[1]} ${vals[2]} $r
done
