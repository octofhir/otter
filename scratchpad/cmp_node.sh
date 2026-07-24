#!/bin/bash
set -euo pipefail
cd /Users/alexanderstreltsov/work/octofhir/otter
B=./target/release/otter-engine-benchmark
TIER="${1:-template}"
S=(method-call-monomorphic numeric-leaf branch-phi dense-array boxed-double-property)
E=(500003500000 -700000 -6000000 5234688 4000000)
printf "%-24s %12s %12s %8s\n" kernel node_jitless "otter_$TIER" ratio
for i in "${!S[@]}"; do
  f="benchmarks/scripts/${S[$i]}.js"
  cp "$f" /tmp/k.mjs
  printf '\nfor(var w=0;w<8;w++)engineKernel();var t=process.hrtime.bigint();for(var i=0;i<20;i++)engineKernel();console.log(Number(process.hrtime.bigint()-t)/20/1e6);\n' >> /tmp/k.mjs
  nj=$(node --jitless /tmp/k.mjs 2>/dev/null | tail -1)
  ot=$("$B" kernel --source "$f" --function engineKernel --expected "${E[$i]}" --jit-tier "$TIER" --samples 20 --warmup 8 2>/dev/null | python3 -c "import sys,json,statistics as st; d=json.load(sys.stdin); m=[x for x in d['metrics'] if x['name']=='wall-time'][0]; print(st.median(m['samples'])/1e6)")
  ratio=$(python3 -c "print(f'{$nj/$ot:.2f}x')")
  printf "%-24s %10.2fms %10.4fms %8s\n" "${S[$i]}" "$nj" "$ot" "$ratio"
done
