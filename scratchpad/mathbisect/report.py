import json
import sys

data = json.load(sys.stdin)
metrics = data.get("metrics") or []
if not metrics:
    print("NO METRIC — checksum mismatch")
else:
    print(f'{metrics[0]["aggregate"]["value"] / 1e6:.3f} ms')
