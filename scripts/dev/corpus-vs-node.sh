#!/usr/bin/env bash
# Compare difftest corpus programs against Node (script semantics, completion
# printed after the event loop drains, like `otter -p`).
# usage: corpus-vs-node.sh <otter-binary> [otter-flags...] -- <corpus files...>
set -u
bin=$1; shift
flags=()
while [ "$#" -gt 0 ] && [ "$1" != "--" ]; do flags+=("$1"); shift; done
shift
for f in "$@"; do
  expected=$(node -e 'const src=require("fs").readFileSync(process.argv[1],"utf8");const v=(0,eval)(src);process.on("exit",()=>console.log(typeof v==="string"?v:String(v)))' "$f" 2>&1)
  actual=$("$bin" ${flags[@]+"${flags[@]}"} -p "$(cat "$f")" 2>&1)
  if [ "$expected" == "$actual" ]; then echo "OK   $(basename "$f")"; else
    echo "DIFF $(basename "$f")"; diff <(echo "$expected") <(echo "$actual") | head -12; fi
done
