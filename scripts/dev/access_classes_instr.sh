#!/usr/bin/env bash
# Retired instructions per access of each runtime-miss class the access census
# found on the Web Tooling Benchmark, for otter and node: each case runs its
# body N times inside one function; the bare loop is subtracted.
# usage: scripts/dev/access_classes_instr.sh <otter-bin> [N] [case...]
O=${1:?binary}; N=${2:-2000000}; shift 2 2>/dev/null || shift $#
D=benchmarks/results/access-classes; mkdir -p "$D"
cat >"$D/prelude.js" <<'JS'
function FakeBuf(n) { const b = new Uint8Array(n); Object.setPrototypeOf(b, FakeBuf.prototype); return b; }
Object.setPrototypeOf(FakeBuf.prototype, Uint8Array.prototype);
const fbuf = FakeBuf(64);
const str = 'abcdefghijklmnopqrstuvwxyz'.repeat(40);
const ikeys = {}; for (let i = 0; i < 256; i++) ikeys[i * 7] = i;
const idict = {}; for (let i = 0; i < 256; i++) idict[i * 7] = i; delete idict[7];
const dict = { a: 1, b: 2, c: 3 }; delete dict.c;
const dproto = Object.create({ get g() { return 1; }, own: 2 }); dproto.x = 1; delete dproto.x;
const nodes = [{ type: 'A', x: 1 }, 'str', { type: 'B', y: 2, x: 3 }, { z: 1, type: 'C' }, { w: 1, v: 2, type: 'D' }, { q: 1, type: 'E' }, { r: 1, type: 'F' }, { s: 1, type: 'G' }];
const wm = new WeakMap(); const wk = {}; wm.set(wk, 1);
function fn1(a) { return a; }
const holes = [1, , 3, , 5, , 7, ,];
const sym = Symbol('s'); const sobj = { [sym]: 1 };
const keep = [];
const mproto = { enter: null, get g() { return 1; } };
const mdicts = []; for (let i = 0; i < 64; i++) { const o = Object.create(mproto); o.a = i; o.b = 1; o['k' + i] = 2; delete o.b; mdicts.push(o); }
const mexports = []; for (let i = 0; i < 64; i++) { const e = {}; Object.defineProperty(e, 'isScope', { enumerable: true, configurable: true, get() { return 1; } }); e.x = 1; e['p' + i] = i; delete e.x; mexports.push(e); }
const megadicts = []; for (let p = 0; p < 16; p++) { const proto = { ['m' + p]: p, enter: null }; for (let i = 0; i < 4; i++) { const o = Object.create(proto); o.a = i; o['k' + i] = 2; o.b = 1; delete o.a; megadicts.push(o); } }
JS
case_body() {
  case $1 in
    empty) echo 's = (s + 1) | 0;' ;;
    tlen) echo 's = (s + fbuf.length) | 0;' ;;
    strchar) echo 's = (s + str[i & 1023].length) | 0;' ;;
    intkey) echo 's = (s + ikeys[(i & 255) * 7]) | 0;' ;;
    intkeydict) echo 's = (s + (idict[(i & 255) * 7] | 0)) | 0;' ;;
    intkeyset) echo 'ikeys[(i & 255) * 7] = s; s = (s + 1) | 0;' ;;
    dictload) echo 's = (s + dict.a) | 0;' ;;
    dictprotoget) echo 's = (s + dproto.g) | 0;' ;;
    dictprotoabsent) echo 's = (s + (dproto.nope === undefined ? 1 : 0)) | 0;' ;;
    megastr) echo 's = (s + (nodes[i & 7].type === "A" ? 1 : 0)) | 0;' ;;
    wmget) echo 's = (s + wm.get(wk)) | 0;' ;;
    fncall) echo 's = (s + fn1.call(null, 1)) | 0;' ;;
    holeload) echo 's = (s + (holes[i & 7] | 0)) | 0;' ;;
    symload) echo 's = (s + sobj[sym]) | 0;' ;;
    megadictabsent) echo 's = (s + (megadicts[i & 63].noScope === undefined ? 1 : 0)) | 0;' ;;
    megadictproto) echo 's = (s + (megadicts[i & 63].enter === null ? 1 : 0)) | 0;' ;;
    mdictabsent) echo 's = (s + (mdicts[i & 63].noScope === undefined ? 1 : 0)) | 0;' ;;
    mdictproto) echo 's = (s + (mdicts[i & 63].enter === null ? 1 : 0)) | 0;' ;;
    mdictprotoget) echo 's = (s + mdicts[i & 63].g) | 0;' ;;
    mdictown) echo 's = (s + mdicts[i & 63].a) | 0;' ;;
    mdictownget) echo 's = (s + mexports[i & 63].isScope) | 0;' ;;
    *) echo "unknown case $1" >&2; exit 1 ;;
  esac
}
CASES=${*:-tlen strchar intkey intkeydict intkeyset dictload dictprotoget dictprotoabsent mdictabsent mdictproto mdictprotoget mdictown mdictownget megadictabsent megadictproto megastr wmget fncall holeload symload}
for c in empty $CASES; do
  { cat "$D/prelude.js"
    echo "function f(n) { let s = 0; for (let i = 0; i < n; i++) { $(case_body $c) } return s; }"
    echo "let out = f(1000); out += f($N);"
    echo "console.log(out);"; } >"$D/$c.js"
done
instr() { /usr/bin/time -l "$@" 2>&1 >/dev/null | awk '/instructions retired/ {print $1}'; }
printf '%-16s %7s %7s %7s\n' case otter node o/node
bo=$(instr "$O" "$D/empty.js"); bn=$(instr node "$D/empty.js")
for c in $CASES; do
  a=$(( ($(instr "$O" "$D/$c.js") - bo) / N )); b=$(( ($(instr node "$D/$c.js") - bn) / N ))
  printf '%-16s %7s %7s %7s\n' $c $a $b $(python3 -c "print(f'{$a/max($b,1):.1f}')")
done
