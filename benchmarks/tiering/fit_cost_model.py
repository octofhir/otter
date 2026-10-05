#!/usr/bin/env python3
"""Reproduce the fixed TierWorkModel coefficients from historical anchors.

Historical compile durations calibrate static compiler geometry. Convert those
costs once to source-opcode work, rounding every coefficient upward. Runtime
admission has no duration, entry bonus, or static-span execution estimate.
Stored holdouts describe the preceding policy; this script does not measure or
validate the new actual-work policy. Standard library only; no source edits.
"""

from __future__ import annotations

import csv
from pathlib import Path


RAW = Path(__file__).resolve().parent / "raw"


def rows(name: str) -> list[dict[str, str]]:
    with (RAW / name).open(newline="", encoding="utf-8") as source:
        return list(csv.DictReader(source, delimiter="\t"))


def round_down_ratio(numerator: int, denominator: int, quantum: int) -> int:
    if numerator < 0 or denominator <= 0 or quantum <= 0:
        raise ValueError("historical anchor must admit a nonnegative coefficient")
    return numerator // denominator // quantum * quantum


def ceil_ratio(numerator: int, denominator: int) -> int:
    if numerator < 0 or denominator <= 0:
        raise ValueError("work conversion requires nonnegative cost and positive scale")
    return numerator // denominator + int(numerator % denominator != 0)


direct = {
    row["tier"]: row
    for row in rows("compile-costs.tsv")
    if row["phase"] == "review-baseline"
}

# These are the historical integer grid anchors, not elapsed runtime inputs.
# The 12/20 ns per-op denominators were conservative prior calibration terms;
# they are an offline conversion choice, not a measured universal opcode cost.
historical_base_ns = {"template": 4_000, "optimizing": 15_000}
historical_register_ns = 40
historical_parameter_ns = 80
offline_ns_per_source_op = {"template": 12, "optimizing": 20}
compile_grid_ns = {"template": 10, "optimizing": 250}
code_base_bytes = {"template": 560, "optimizing": 320}
code_grid_bytes = {"template": 2, "optimizing": 1}
code_per_register_bytes = 16
historical_memory_charge_ns_per_byte = 4
retained_generated_code_limit_bytes = 64 * 1024 * 1024

coefficients: dict[str, int] = {}
for tier in ("template", "optimizing"):
    anchor = direct[tier]
    instructions = int(anchor["bytecode_instructions"])
    instruction_ns = round_down_ratio(
        int(anchor["compile_duration_ns"]) - historical_base_ns[tier],
        instructions,
        compile_grid_ns[tier],
    )
    scale = offline_ns_per_source_op[tier]
    coefficients.update({
        f"{tier}_compile_base_work": ceil_ratio(historical_base_ns[tier], scale),
        f"{tier}_compile_per_instruction_work": ceil_ratio(instruction_ns, scale),
        f"{tier}_compile_per_register_work": ceil_ratio(historical_register_ns, scale),
        f"{tier}_compile_per_parameter_work": ceil_ratio(historical_parameter_ns, scale),
        f"{tier}_code_base_bytes": code_base_bytes[tier],
        f"{tier}_code_per_instruction_bytes": round_down_ratio(
            int(anchor["code_bytes"]) - code_base_bytes[tier],
            instructions,
            code_grid_bytes[tier],
        ),
    })
    # 4 ns/byte divided by 12 or 20 ns/op is exactly 1/3 or 1/5
    # work/byte. Runtime charges ceil(predicted code bytes / this divisor).
    assert scale % historical_memory_charge_ns_per_byte == 0
    coefficients[f"{tier}_memory_work_code_bytes_divisor"] = (
        scale // historical_memory_charge_ns_per_byte
    )
coefficients["code_per_register_bytes"] = code_per_register_bytes


def estimated_code_bytes(tier: str, instructions: int, registers: int) -> int:
    """Predicted executable geometry, not measured retained resource usage."""
    if min(instructions, registers) < 0:
        raise ValueError("static geometry must be nonnegative")
    return (
        coefficients[f"{tier}_code_base_bytes"]
        + coefficients[f"{tier}_code_per_instruction_bytes"] * instructions
        + code_per_register_bytes * registers
    )


def minimum_required_work(tier: str, instructions: int, registers: int,
                          parameters: int, previous_attempts: int = 0) -> int:
    """Static geometry estimate, with no historical execution observation.

    Runtime admission additionally requires estimated_code_bytes to fit its
    available_code_bytes input. This offline census has no actual retained
    GeneratedCodeBytes lease samples and cannot reconstruct that headroom.
    """
    if min(instructions, registers, parameters, previous_attempts) < 0:
        raise ValueError("static geometry and compiler attempts must be nonnegative")
    compile_work = (
        coefficients[f"{tier}_compile_base_work"]
        + coefficients[f"{tier}_compile_per_instruction_work"] * instructions
        + coefficients[f"{tier}_compile_per_register_work"] * registers
        + coefficients[f"{tier}_compile_per_parameter_work"] * parameters
    )
    code_bytes = estimated_code_bytes(tier, instructions, registers)
    memory_work = ceil_ratio(code_bytes, coefficients[f"{tier}_memory_work_code_bytes_divisor"])
    return compile_work * (previous_attempts + 1) + memory_work + 1


def passes(row: dict[str, str]) -> bool:
    value = float(row["value"])
    reference = float(row["reference"])
    noise = float(row["noise_percent"])
    if row["direction"] == "higher":
        return value >= reference * (1 - noise / 100)
    if row["direction"] == "lower":
        return value <= reference * (1 + noise / 100)
    raise ValueError(f"unknown historical metric direction: {row['direction']}")


# Check the stored earlier-policy observations against their recorded envelopes.
# This is neither a new-policy holdout nor a refit on leave-one-workload-out data.
holdouts = [row for row in rows("policy-holdouts.tsv") if row["candidate"] == "step6-final"]
assert holdouts, "missing historical policy observations"
for historical in holdouts:
    assert passes(historical), f"historical holdout record exceeds its envelope: {historical}"

capacity = rows("capacity-cdf.tsv")
property_capacity = min(
    int(row["upper_bound"])
    for row in capacity
    if row["family"] == "property" and float(row["cumulative_percent"]) >= 99.9
)
call_capacity = 8  # Four is right-censored and failed the historical holdout.

for name, value in coefficients.items():
    print(f"{name}={value}")
for tier, scale in offline_ns_per_source_op.items():
    print(f"calibration_{tier}_ns_per_source_op={scale}")
for tier, anchor in direct.items():
    instructions, registers, parameters = (
        int(anchor[key]) for key in ("bytecode_instructions", "registers", "parameters")
    )
    print(f"static_anchor_{tier}_geometry=N{instructions},R{registers},P{parameters},A0")
    print(f"static_anchor_{tier}_estimated_code_bytes="
          f"{estimated_code_bytes(tier, instructions, registers)}")
    print(f"static_anchor_{tier}_minimum_required_work="
          f"{minimum_required_work(tier, instructions, registers, parameters)}")
print(f"profiled_property_pic_capacity={property_capacity}")
print(f"profiled_call_target_capacity={call_capacity}")
print(f"retained_generated_code_limit_bytes={retained_generated_code_limit_bytes}")
print("resource_admission_input=available_code_bytes")
print("historical_available_code_bytes=unmeasured")
print(f"historical_holdout_records={len(holdouts)}/{len(holdouts)} pass")
print("current_policy_performance=unmeasured")
print("current_policy_holdout=unmeasured")
