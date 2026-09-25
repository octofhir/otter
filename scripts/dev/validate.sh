#!/usr/bin/env bash
cd "$(dirname "$0")/../.."
S=scripts/dev
R=benchmarks/results/validate; mkdir -p $R
tag=${1:-store}
bash scripts/dev/jitset.sh aarch64 ${LEVELS_A:-unset 16 4 1}
bash scripts/dev/jitset.sh x86 ${LEVELS_X:-unset 16 4 1}
cargo test -q -p otter-jit --lib machine 2>&1 | grep -E "test result|FAILED|panicked" | head
cargo test -q -p otter-jit --target x86_64-apple-darwin --lib machine 2>&1 | grep -E "test result|FAILED|panicked" | head
cargo build --release -q -p otter-test262 2>&1 | grep -v future | head -2
cargo build --release -q --target x86_64-apple-darwin -p otter-test262 2>&1 | grep -v future | head -2
for arch in native x86; do
  bin=target/release/otter-test262; pre=""
  [ $arch = x86 ] && bin=target/x86_64-apple-darwin/release/otter-test262 && pre="arch -x86_64"
  tp=0; tf=0
  for s in ${EXTRA_SECTIONS:-} language/expressions/property-accessors/ language/expressions/class/ language/statements/class/ language/expressions/new/ language/expressions/super/ language/statements/for-in/ built-ins/Object/defineProperty/ built-ins/Object/create/ built-ins/Proxy/set/ built-ins/Proxy/get/ language/expressions/call/ language/expressions/assignment/; do
    n=$(echo $s | tr '/' '-')
    env -u OTTER_GC_STRESS $pre $bin run --filter $s --jit-tier production-tiered --output $R/t262-$tag-$arch-$n.json > $R/t262-$tag-$arch-$n.log 2>&1
    python3 -c "
import json,sys;d=json.load(open('$R/t262-$tag-$arch-$n.json'));t=d['totals']
bad=t['failed']+t['crashed']+t['timed_out']
print('$arch $s', t)
[print('  FAIL',f if isinstance(f,str) else f.get('path',f)) for f in d['failing_tests'][:10]]
"
  done
done
git diff --check && echo diffcheck-ok
CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh > $R/gate-$tag.log 2>&1; echo "gate exit=$?"; tail -3 $R/gate-$tag.log
