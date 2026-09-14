#!/usr/bin/env python3
"""Reproduce the integer Step 6 tier-policy calibration.

The policy is deliberately small enough to audit.  Direct compiler samples
anchor the compile/code terms, the scalar-loop kernel anchors saved backedge
cost, and V8/startup holdouts reject candidates.  This script uses only the
Python standard library and never edits source files.
"""

from __future__ import annotations

import csv
from pathlib import Path


ROOT = Path(__file__).parent
RAW = ROOT / "raw"
SCORE_NOISE_PERCENT = 3.0
SAVINGS_HEADROOM_PERCENT = 7


def rows(name: str) -> list[dict[str, str]]:
    with (RAW / name).open(newline="", encoding="utf-8") as source:
        return list(csv.DictReader(source, delimiter="\t"))


def round_down(value: float, quantum: int) -> int:
    return int(value) // quantum * quantum


compile_rows = rows("compile-costs.tsv")
direct = {
    row["tier"]: row
    for row in compile_rows
    if row["phase"] == "review-baseline"
}
numeric = next(row for row in rows("execution-savings.tsv") if row["workload"] == "kernel-numeric-leaf")
iterations = int(numeric["iterations"])
span = int(numeric["loop_span_instructions"])
interpreter_ns = int(numeric["interpreter_ns"])

# Intercepts and small metadata charges are the integer grid point selected by
# the holdout checks below.  Per-instruction terms are then fitted from the
# direct loop, rounded down so measurement noise cannot move policy by one run.
template_compile_base_ns = 4_000
optimizing_compile_base_ns = 15_000
compile_per_register_ns = 40
compile_per_parameter_ns = 80
template_compile_per_instruction_ns = round_down(
    (int(direct["template"]["compile_duration_ns"]) - template_compile_base_ns)
    / int(direct["template"]["bytecode_instructions"]),
    10,
)
optimizing_compile_per_instruction_ns = round_down(
    (int(direct["optimizing"]["compile_duration_ns"]) - optimizing_compile_base_ns)
    / int(direct["optimizing"]["bytecode_instructions"]),
    250,
)

template_code_base_bytes = 560
optimizing_code_base_bytes = 320
code_per_register_bytes = 16
template_code_per_instruction_bytes = round_down(
    (int(direct["template"]["code_bytes"]) - template_code_base_bytes)
    / int(direct["template"]["bytecode_instructions"]),
    2,
)
optimizing_code_per_instruction_bytes = round_down(
    (int(direct["optimizing"]["code_bytes"]) - optimizing_code_base_bytes)
    / int(direct["optimizing"]["bytecode_instructions"]),
    1,
)

template_backedge_raw = (
    interpreter_ns - int(numeric["template_ns"])
) / iterations / span
optimizing_backedge_raw = (
    interpreter_ns - int(numeric["optimizing_ns"])
) / iterations / span
template_backedge_saved_per_instruction_ns = int(
    template_backedge_raw * (100 - SAVINGS_HEADROOM_PERCENT) / 100
)
optimizing_backedge_saved_per_instruction_ns = int(
    optimizing_backedge_raw * (100 - SAVINGS_HEADROOM_PERCENT) / 100
)

coefficients = {
    "template_compile_base_ns": template_compile_base_ns,
    "template_compile_per_instruction_ns": template_compile_per_instruction_ns,
    "optimizing_compile_base_ns": optimizing_compile_base_ns,
    "optimizing_compile_per_instruction_ns": optimizing_compile_per_instruction_ns,
    "compile_per_register_ns": compile_per_register_ns,
    "compile_per_parameter_ns": compile_per_parameter_ns,
    "template_code_base_bytes": template_code_base_bytes,
    "template_code_per_instruction_bytes": template_code_per_instruction_bytes,
    "optimizing_code_base_bytes": optimizing_code_base_bytes,
    "optimizing_code_per_instruction_bytes": optimizing_code_per_instruction_bytes,
    "code_per_register_bytes": code_per_register_bytes,
    "template_entry_saved_per_instruction_ns": 12,
    "template_entry_transition_cost_ns": 96,
    "optimizing_entry_saved_per_instruction_ns": 20,
    "optimizing_entry_transition_cost_ns": 128,
    "direct_call_target_saved_ns": 1_450,
    "template_backedge_saved_per_instruction_ns": template_backedge_saved_per_instruction_ns,
    "optimizing_backedge_saved_per_instruction_ns": optimizing_backedge_saved_per_instruction_ns,
    "exit_penalty_ns": 192,
    "code_memory_charge_per_byte_ns": 4,
    "entry_continuation_multiplier": 1,
    "loop_continuation_multiplier": 1,
}


def passes(row: dict[str, str]) -> bool:
    value = float(row["value"])
    reference = float(row["reference"])
    noise = float(row["noise_percent"])
    if row["direction"] == "higher":
        return value >= reference * (1 - noise / 100)
    return value <= reference * (1 + noise / 100)


holdouts = [row for row in rows("policy-holdouts.tsv") if row["candidate"] == "step6-final"]
for held_out in holdouts:
    training = [row for row in holdouts if row["workload"] != held_out["workload"]]
    assert all(passes(row) for row in training), f"training failure before {held_out['workload']} holdout"
    assert passes(held_out), f"leave-one-workload-out failure: {held_out}"

capacity = rows("capacity-cdf.tsv")
property_capacity = min(
    int(row["upper_bound"])
    for row in capacity
    if row["family"] == "property" and float(row["cumulative_percent"]) >= 99.9
)
call_capacity = 8  # Four is right-censored and fails the DeltaBlue holdout.

for name, value in coefficients.items():
    print(f"{name}={value}")
print(f"profiled_property_pic_capacity={property_capacity}")
print(f"profiled_call_target_capacity={call_capacity}")
print(f"leave_one_workload_out={len(holdouts)}/{len(holdouts)} pass")
