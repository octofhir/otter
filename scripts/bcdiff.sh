#!/usr/bin/env bash
# Bytecode diff against V8 Ignition.
#
# Node prints its bytecode for the same source we compile. Comparing the two
# op streams for one function shows work we emit that V8 does not: dead
# completion-value stores, redundant loads, unfused numeric pairs. It is the
# cheapest source of interpreter wins because it needs no profiler — the
# extra instructions are visible in the listing.
#
# Usage:
#   scripts/bcdiff.sh dense-array               # both listings + op counts
#   scripts/bcdiff.sh dense-array engineKernel  # restrict to one function
set -euo pipefail

cd "$(dirname "$0")/.."

kernel="${1:?usage: bcdiff.sh <kernel-name> [function]}"
function_name="${2:-engineKernel}"
source_path="benchmarks/scripts/${kernel}.js"
[[ -f "$source_path" ]] || { echo "no such kernel: $source_path" >&2; exit 1; }

OTTER=target/release/otter
[[ -x "$OTTER" ]] || cargo build --release -q -p otter-cli

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

printf '=== V8 Ignition: %s ===\n' "$function_name"
# --print-bytecode needs the function to actually run; jitless keeps V8 in
# Ignition so the listing is the interpreter's real op stream.
cp "$source_path" "$work/k.js"
printf '\nengineKernel();\n' >>"$work/k.js"
node --jitless --print-bytecode \
  --print-bytecode-filter="$function_name" "$work/k.js" 2>/dev/null |
  sed -n '/Bytecode length/,/Constant pool/p' |
  tee "$work/v8.txt"

printf '\n=== Otter: %s ===\n' "$function_name"
"$OTTER" --dump-bytecode "$source_path" 2>/dev/null |
  awk -v fn="$function_name" '
    /^function / { active = ($2 == fn) }
    active { print }
  ' | tee "$work/otter.txt"

printf '\n=== op counts ===\n'
v8_ops=$(grep -cE '@ +[0-9]+ :' "$work/v8.txt" || true)
otter_ops=$(grep -cE '^ +[0-9]{6}: ' "$work/otter.txt" || true)
printf 'v8-ignition %s\notter       %s\n' "${v8_ops:-0}" "${otter_ops:-0}"
if [[ "${v8_ops:-0}" -gt 0 && "${otter_ops:-0}" -gt 0 ]]; then
  python3 -c "print(f'ratio       {$otter_ops / $v8_ops:.2f}x')"
fi

cat <<'NOTE'

Read the two listings side by side. Ops we emit and V8 does not are the
work list; each one that disappears is a win in every tier at once, because
the baseline tiers compile this stream directly.
NOTE
