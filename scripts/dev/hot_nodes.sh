#!/usr/bin/env bash
# Hot graph nodes of one fixture: a run with perf map and artifacts, sampled
# on its isolate thread, attributed to asm.txt regions; the node kinds are
# summed too.
# usage: scripts/dev/hot_nodes.sh <binary> <fixture.js> <out-dir> [secs] [delay]
O=${1:?binary}; JS=${2:?fixture}; OUT=${3:?out}; SECS=${4:-2}; DELAY=${5:-0.4}
rm -rf "$OUT"; mkdir -p "$OUT"
"$O" --perf-basic-prof --jit-artifacts="$OUT/art" "$JS" >/dev/null &
pid=$!
sleep "$DELAY"
sample "$pid" "$SECS" 1 -file "$OUT/sample.txt" >/dev/null 2>&1
wait "$pid"
cp "/tmp/perf-$pid.map" "$OUT/perf.map"
python3 "$(dirname "$0")/jit_hot_regions.py" "$OUT/sample.txt" "$OUT/perf.map" "$OUT/art" 400 > "$OUT/regions.txt"
head -40 "$OUT/regions.txt"
echo "--- by node kind"
python3 - "$OUT/regions.txt" <<'PY'
import re,sys,collections
c=collections.Counter();tot=0
for l in open(sys.argv[1]):
    m=re.match(r'\s*(\d+)\s+[\d.]+%\s+(.*)',l)
    if not m: continue
    n=int(m.group(1)); s=m.group(2); tot+=n
    k=re.search(r'\] v\d+ (\w+)',s)
    key=k.group(1) if k else (s.split(' (')[0] if s.startswith('routine') else re.sub(r'.*\] ','',s))
    c[key]+=n
for k,v in c.most_common(30): print(f'{v:6} {100*v/max(tot,1):5.1f}% {k}')
PY
