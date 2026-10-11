#!/usr/bin/env python3
"""Attribute generated-code self samples to the current call/frame contract.

Usage: frame_profile.py ARTIFACTS PROFILE --output RESULTS_PREFIX [--top 8]

PROFILE is an OFFSETS=1 launchprof report from the same process as ARTIFACTS.
Only rows between ``== self`` and the next section contribute. ARM64 sampled
offsets are rounded down with ``& ~3`` before exact half-open interval lookup;
inclusive counts and a next-instruction tolerance never enter the totals.

The code map owns body attribution. Current ARM64 assembly supplies the frame
spans absent from its structural map: tier entry, private JS entry, their cold
branches, ordinary return and constructor return. Entry instruction causes use
the shared ARM64 frame emitter's current field layout. Unknown instructions
remain explicit, so a changed contract cannot silently inherit old counts.
Static instruction counts describe emitted code, not retired instructions or
execution frequency; sampled time cannot measure dynamic instruction counts.
"""

import argparse
import collections
from dataclasses import dataclass
import json
from pathlib import Path
import re


SELF_ROW = re.compile(r"^\s*(\d+)\s+(.+?)\s*$")
JS_ROW = re.compile(r"^JS:(.*) \[([\w-]+) c(\d+)\]\+0x([0-9a-fA-F]+)$")
ASM_ROW = re.compile(r"^\+0x([0-9a-fA-F]+):\s+(?:[0-9a-fA-F]{8}\s+)?(.*)$")
TARGET = re.compile(r"\bL([0-9a-fA-F]{8})\b")

LABELS = {
    "body": "Тело / инструкции IR",
    "call-entry": "Private JS entry",
    "tier-prologue": "Tier entry / OSR",
    "return": "Обычный возврат",
    "construct-return": "Constructor completion / возврат",
    "entry-cold:staged": "Холодный staged entry",
    "entry-cold:underarity": "Холодная underarity",
    "entry-cold:construct": "Холодный construct",
    "entry-cold:prepare": "Холодная receiver preparation",
    "entry-cold:overflow": "Холодный stack overflow",
    "entry-cold": "Прочая холодная entry-работа",
    "cold": "Холодные exits / helpers / literals",
    "outside": "Неразмеченное вне тела",
    "missing-artifact": "Нет артефакта",
    "invalid-offset": "Offset вне code.bin",
    "deferred-publication": "Публикация отложенного frame на VM boundary",
}

CAUSES = {
    "receiver-checks": "Receiver proof / preparation check",
    "frame-save": "Сохранение frame-регистров",
    "stack-check": "Резервирование и проверка стека",
    "root-record": "Инициализация roots / scratch",
    "source-owner": "Source owner / parent frame",
    "call-input": "Сохранение call inputs / actual span",
    "window": "Register window / копирование / заполнение",
    "continuation": "Continuation / staged request",
    "deferred-marker": "Маркер отложенной публикации",
    "publication": "Публикация frame",
    "underarity-check": "Underarity check",
    "construct-check": "Construct check",
    "tiering": "Tiering / source-work accounting",
    "unclassified": "Не классифицировано",
}


@dataclass(frozen=True)
class Instruction:
    offset: int
    size: int
    text: str


@dataclass(frozen=True)
class Span:
    start: int
    end: int
    kind: str

    def contains(self, offset):
        return self.start <= offset < self.end


def parse_profile(path):
    report = path.read_text()
    headline = re.search(r"^samples on (.*?): (\d+) over (\d+) runs$", report, re.M)
    rows = []
    in_self = False
    found_self = False
    for line in report.splitlines():
        if line == "== self":
            if found_self:
                raise ValueError("More than one self section; use one aggregated report")
            in_self = found_self = True
            continue
        if line.startswith("=="):
            in_self = False
        if not in_self or not line.strip():
            continue
        match = SELF_ROW.fullmatch(line)
        if not match:
            raise ValueError(f"Malformed self row: {line!r}")
        rows.append((int(match[1]), match[2]))
    if not found_self:
        raise ValueError("Missing == self section; refuse to include inclusive rows")
    observed = sum(count for count, _ in rows)
    reported = int(headline[2]) if headline else None
    return rows, {
        "thread": headline[1] if headline else None,
        "runs": int(headline[3]) if headline else None,
        "reportedSelfSamples": reported,
        "listedSelfSamples": observed,
        "completeSelfListing": reported is None or reported == observed,
    }


def parse_assembly(path):
    result = []
    for line in path.read_text().splitlines():
        match = ASM_ROW.fullmatch(line)
        if not match:
            continue
        text = match[2]
        width = re.search(r"encoded-bytes=(\d+)", text)
        result.append(Instruction(int(match[1], 16), int(width[1]) if width else 4, text))
    result.sort(key=lambda instruction: instruction.offset)
    if len({instruction.offset for instruction in result}) != len(result):
        raise ValueError(f"Repeated assembly offsets: {path}")
    return result


def branch_target(instruction):
    match = TARGET.search(instruction.text)
    return int(match[1], 16) if match else None


def entry_causes(instructions):
    """Label structural entry phases without depending on code offsets."""
    result = {}
    cold = {}
    save = next((i for i, instruction in enumerate(instructions)
                 if instruction.text.startswith("stp x29, x30, [sp, #-")), None)
    if save is None:
        return result, cold, None
    for instruction in instructions[:save]:
        result[instruction.offset] = "receiver-checks"
    phase = "stack-check"
    previous = ""
    window_started = False
    window_finished = False
    for index, instruction in enumerate(instructions[save:], save):
        text = instruction.text
        target = branch_target(instruction)
        cause = None
        if index < save + 4 or re.match(r"(?:stp|stur) x2[2-8], .*\[x29, #-", text):
            cause = "frame-save"
        elif text == "mov x20, x0":
            cause = "call-input"
        elif text.startswith(("sub x9, sp,", "ldr x11, [x20, #0x28]", "cmp x9, x11",
                              "mov sp, x9", "sub x21, x29,")):
            cause = "stack-check"
        elif text.startswith("b.cc ") and previous == "cmp x9, x11":
            cause = "stack-check"
            cold[target] = "overflow"
        elif text.startswith("str xzr, [sp"):
            cause = "root-record"
        elif text.startswith(("mov w16, #0xffffffff", "stp x16, xzr, [x21, #0x60]")):
            cause = "continuation"
        elif text.startswith("tbnz w8, #0,"):
            cause = "continuation"
            cold[target] = "staged"
        elif text.startswith(("ldr x10, [x20, #0x8]", "str x10, [x29, #0x28]",
                              "mov x13,", "ldr x14, L", "stp x13, x14, [x21]")):
            cause = "source-owner"
        elif text.startswith(("stp x2, x3, [x21, #0x20]", "stp x1, x4, [x21, #0x30]",
                              "add x15, x29, #0x30", "stp x15, x10, [x21, #0x40]",
                              "add x19, x29, #0x30")):
            cause = "call-input"
            if text == "add x19, x29, #0x30":
                window_finished = True
        elif text == "mov x16, #0x2" or (text == "str x16, [x29, #0x28]" and previous == "mov x16, #0x2"):
            cause = "deferred-marker"
        elif text.startswith(("mov x16, #0xffffff", "mov x17, sp", "stp x16, x17, [x21, #0x50]")):
            cause = "root-record"
            if text.startswith("stp x16, x17"):
                window_started = True
        elif text == "str x21, [x20, #0x8]":
            cause = "publication"
            window_finished = True
        elif text.startswith("cmp x3, #0xa") or (text.startswith("b.ne ") and previous == "cmp x3, #0xa"):
            cause = "construct-check"
            if target is not None:
                cold[target] = "construct"
        elif text.startswith("cbnz x5,"):
            cause = "receiver-checks"
            cold[target] = "prepare"
        elif window_finished and (text.startswith("cmp w4,") or
                                   (text.startswith("b.cc ") and previous.startswith("cmp w4,"))):
            cause = "underarity-check"
            if target is not None:
                cold[target] = "underarity"
        elif (window_started and not window_finished) or text.startswith((
                "stp xzr, xzr, [x21, #0x10]", "stp x19,", "mov x19,", "add x19, x9,",
                "mov x14,")):
            cause = "window"
        elif "sourceWorkCell" in text or re.search(r"\[x8, #0x", text):
            cause = "tiering"
            phase = "tiering"
        elif phase == "tiering" and not text.startswith("ldr x10, [x20"):
            cause = "tiering"
        result[instruction.offset] = cause or "unclassified"
        previous = text
    return result, {offset: kind for offset, kind in cold.items() if offset is not None}, instructions[save].offset


class CodeObject:
    def __init__(self, directory):
        self.directory = directory
        self.manifest = json.loads((directory / "manifest.json").read_text())
        self.map = json.loads((directory / "code-map.json").read_text())
        self.instructions = parse_assembly(directory / "asm.txt")
        self.regions = [region for region in self.map["regions"]
                        if region["startOffset"] < region["endOffset"]]
        body = [region for region in self.map["regions"] if region["kind"] == "instruction"]
        self.body_start = min((region["startOffset"] for region in body), default=None)
        self.body_end = max((region["endOffset"] for region in body), default=None)
        self.spans = []
        self.publication_spans = []
        self.causes = {}
        self.frame_start = None
        for entry in self.map.get("osrEntries", []):
            self.spans.append(Span(entry["startOffset"], entry["endOffset"], "tier-prologue"))
        call = self.map.get("callEntryOffset")
        if self.manifest["architecture"] != "aarch64" or self.body_start is None:
            return
        if call is not None and call < self.body_start:
            entry = [instruction for instruction in self.instructions
                     if call <= instruction.offset < self.body_start]
            self.causes, cold, self.frame_start = entry_causes(entry)
            self.spans.append(Span(call, self.body_start, "call-entry"))
            cold = sorted((offset, kind) for offset, kind in cold.items() if offset < call)
            cold_start = cold[0][0] if cold else call
            tier = self.map["entryOffset"]
            if tier < cold_start:
                self.spans.append(Span(tier, cold_start, "tier-prologue"))
            for index, (start, kind) in enumerate(cold):
                end = cold[index + 1][0] if index + 1 < len(cold) else call
                self.spans.append(Span(start, end, f"entry-cold:{kind}"))
        # Near Graph returns follow the body. Template maps already give
        # return regions; only infer a Graph span when its exact start fits.
        tail = [instruction for instruction in self.instructions if instruction.offset >= self.body_end]
        constructor_check = 3 if len(tail) > 2 and tail[2].text.startswith("tbnz w9, #1,") else 2
        if len(tail) > constructor_check and tail[0].offset == self.body_end and tail[0].text == "mov x1, #0x0" \
                and tail[1].text == "ldr x9, [x29, #0x28]" \
                and tail[constructor_check].text.startswith("tbnz w9, #0,"):
            returns = [instruction.offset + instruction.size for instruction in tail if instruction.text == "ret"]
            if returns:
                self.spans.append(Span(self.body_end, returns[0], "return"))
            if len(returns) > 1:
                self.spans.append(Span(returns[0], returns[1], "construct-return"))
        # Publication has a distinct bit-one guard and exact success label.
        # Keep this as an orthogonal subset of primary attribution: the
        # containing IR operation / cold entry remains its source owner.
        for index, instruction in enumerate(self.instructions[:-1]):
            guard = self.instructions[index + 1]
            if instruction.text != "ldr x16, [x29, #0x28]" or not guard.text.startswith("tbz w16, #1,"):
                continue
            end = branch_target(guard)
            if end is None or end <= guard.offset or end > self.manifest["codeBytes"]:
                continue
            block = [item for item in self.instructions if instruction.offset <= item.offset < end]
            if block[-1].text == "str x21, [x20, #0x8]":
                span = Span(instruction.offset, end, "deferred-publication")
                if any(span.start < previous.end and previous.start < span.end for previous in self.publication_spans):
                    raise ValueError(f"Overlapping publication spans: {directory}")
                self.publication_spans.append(span)

    def attribute(self, offset):
        if not 0 <= offset < self.manifest["codeBytes"]:
            return "invalid-offset", None
        regions = [region for region in self.regions
                   if region["startOffset"] <= offset < region["endOffset"]]
        hit = min(regions, key=lambda region: region["endOffset"] - region["startOffset"], default=None)
        if hit and hit["kind"] == "instruction":
            return "body", hit
        for span in self.spans:
            if span.contains(offset):
                return span.kind, hit
        if hit:
            kind = hit["kind"]
            if kind == "returnEpilogue":
                return "return", hit
            if kind == "entryPrologue":
                return "entry-cold", hit
            return "cold", hit
        return "outside", None


def rows(counter, denominator):
    return [{"kind": kind, "samples": count, "percent": round(100 * count / max(denominator, 1), 3)}
            for kind, count in counter.most_common()]


def analyze(artifacts, profile, top):
    objects = {}
    for directory in sorted(artifacts.glob("jit-*")):
        if directory.is_dir() and (directory / "manifest.json").exists():
            obj = CodeObject(directory)
            code = obj.manifest["codeObjectId"]
            if code in objects:
                raise ValueError(f"Repeated codeObjectId {code}")
            objects[code] = obj
    sample_rows, provenance = parse_profile(profile)
    native = collections.Counter()
    aggregate = collections.Counter()
    per_code = collections.defaultdict(collections.Counter)
    per_cause = collections.Counter()
    sampled_instructions = collections.defaultdict(collections.Counter)
    publication = collections.Counter()
    publication_by_code = collections.Counter()
    missing = set()
    for count, name in sample_rows:
        match = JS_ROW.fullmatch(name)
        if not match:
            if name.startswith("JS:"):
                raise ValueError("Generated self row has no code offset; enable OFFSETS=1")
            native[name] += count
            continue
        code, raw = int(match[3]), int(match[4], 16)
        obj = objects.get(code)
        if obj is None:
            missing.add(code)
            kind = "missing-artifact"
            offset = raw & ~3
        else:
            if match[2] != obj.manifest["tier"]:
                raise ValueError(f"Profile/artifact tier mismatch for c{code}")
            offset = raw & ~3 if obj.manifest["architecture"] == "aarch64" else raw
            kind, _ = obj.attribute(offset)
            if any(span.contains(offset) for span in obj.publication_spans):
                publication[kind] += count
                publication_by_code[code] += count
            if kind == "call-entry":
                per_cause[obj.causes.get(offset, "unclassified")] += count
        aggregate[kind] += count
        per_code[code][kind] += count
        sampled_instructions[code][offset] += count
    generated = sum(aggregate.values())
    code_reports = []
    for code, counts in sorted(per_code.items(), key=lambda item: (-sum(item[1].values()), item[0])):
        if code not in objects:
            continue
        obj = objects[code]
        report = {"codeObjectId": code, "functionId": obj.manifest["functionId"],
                  "functionName": obj.manifest["functionName"], "tier": obj.manifest["tier"],
                  "bytecodeBytes": obj.manifest["bytecodeBytes"], "selfSamples": sum(counts.values()),
                  "spans": rows(counts, generated), "directory": str(obj.directory),
                  "entryOffset": obj.map["entryOffset"], "callEntryOffset": obj.map.get("callEntryOffset"),
                  "bodyStart": obj.body_start, "bodyEnd": obj.body_end,
                  "inlinedFunctions": sorted({region["functionId"] for region in obj.map["regions"]
                                              if region["kind"] == "instruction"} - {obj.manifest["functionId"]}),
                  "inferredSpans": [vars(span) for span in obj.spans]}
        report["deferredPublicationSpans"] = [vars(span) for span in obj.publication_spans]
        report["deferredPublicationSelfSamples"] = publication_by_code[code]
        entry = [instruction for instruction in obj.instructions if instruction.offset in obj.causes]
        report["entryInstructions"] = [{**vars(instruction), "cause": obj.causes[instruction.offset],
                                        "selfSamples": sampled_instructions[code][instruction.offset]}
                                       for instruction in entry]
        report["entryEmittedInstructions"] = sum(instruction.size // 4 for instruction in entry)
        report["frameEmittedInstructions"] = sum(instruction.size // 4 for instruction in entry
                                                 if obj.frame_start is not None and instruction.offset >= obj.frame_start)
        report["entryCauses"] = dict(collections.Counter(obj.causes[instruction.offset] for instruction in entry
                                                          for _ in range(instruction.size // 4)))
        code_reports.append(report)
    tiny = [report for report in code_reports if report["tier"] == "optimizing"
            and report["bytecodeBytes"] <= 200 and not report["inlinedFunctions"]][:top]
    return {"profile": str(profile), "artifacts": str(artifacts), **provenance,
            "generatedSelfSamples": generated, "nativeSelfSamples": sum(native.values()),
            "missingCodeObjects": sorted(missing), "spanTotals": rows(aggregate, generated),
            "callEntryCauseSamples": rows(per_cause, generated), "codeObjects": code_reports,
            "deferredPublicationSubset": rows(publication, generated),
            "tinyNonInlineFunctions": [report["codeObjectId"] for report in tiny],
            "nativeTop": [{"symbol": symbol, "samples": count} for symbol, count in native.most_common(top)]}


def markdown(result):
    total = result["listedSelfSamples"]
    generated = result["generatedSelfSamples"]
    lines = ["# Публикация кадров: self-профиль", "",
             f"Всего {total} self samples; generated {generated} ({100 * generated / max(total, 1):.1f}%).",
             "Учитывается только `== self`; ARM64 PC округлён `& ~3`, интервалы строго `[start, end)`.",
             "Статические количества ниже — emitted instructions; частоту выполнения и retired instructions они не измеряют.", "",
             "| Участок | Self | % generated | % всего |", "|---|---:|---:|---:|"]
    for row in result["spanTotals"]:
        lines.append(f"| {LABELS.get(row['kind'], row['kind'])} | {row['samples']} | {row['percent']:.1f} | "
                     f"{100 * row['samples'] / max(total, 1):.1f} |")
    lines.extend(["", "## Причины self samples в private JS entry", "",
                  "| Причина | Self | % generated |", "|---|---:|---:|"])
    for row in result["callEntryCauseSamples"]:
        lines.append(f"| {CAUSES[row['kind']]} | {row['samples']} | {row['percent']:.1f} |")
    if result["deferredPublicationSubset"]:
        lines.extend(["", "## Отложенная публикация на VM boundary", "",
                      "Подмножество основной разметки, повторно к общему числу samples не добавляется.", "",
                      "| Участок-владелец | Self | % generated |", "|---|---:|---:|"])
        for row in result["deferredPublicationSubset"]:
            lines.append(f"| {LABELS.get(row['kind'], row['kind'])} | {row['samples']} | {row['percent']:.1f} |")
    lines.extend(["", "## Горячие маленькие функции без инлайнов", "",
                  "| Code / function | Bytecode bytes | Self | Private entry emitted | Frame part emitted |",
                  "|---|---:|---:|---:|---:|"])
    tiny = set(result["tinyNonInlineFunctions"])
    for report in result["codeObjects"]:
        if report["codeObjectId"] in tiny:
            lines.append(f"| c{report['codeObjectId']} / f{report['functionId']} `{report['functionName']}` | "
                         f"{report['bytecodeBytes']} | {report['selfSamples']} | {report['entryEmittedInstructions']} | "
                         f"{report['frameEmittedInstructions']} |")
    lines.extend(["", "Frame part начинается с сохранения frame-регистров. Sloppy receiver proof entry ещё выполняет "
                  "`mov x5, xzr` перед этой частью. Emitted counts включают обе ветви копирования window; "
                  "они не являются числом инструкций одного вызова."])
    for report in result["codeObjects"]:
        if report["codeObjectId"] not in tiny:
            continue
        lines.extend(["", f"### c{report['codeObjectId']} / f{report['functionId']}", "",
                      "| Причина | Emitted | Self |", "|---|---:|---:|"])
        sampled = collections.Counter()
        for instruction in report["entryInstructions"]:
            sampled[instruction["cause"]] += instruction["selfSamples"]
        for cause, count in report["entryCauses"].items():
            lines.append(f"| {CAUSES[cause]} | {count} | {sampled[cause]} |")
        lines.extend(["", "```asm"])
        for instruction in report["entryInstructions"]:
            lines.append(f"+0x{instruction['offset']:08x}: {instruction['text']} "
                         f"; {instruction['cause']}, self={instruction['selfSamples']}")
        lines.append("```")
    if not result["completeSelfListing"]:
        lines.extend(["", "Self-таблица усечена: проценты относятся к видимым строкам, не ко всему профилю."])
    if result["missingCodeObjects"]:
        lines.extend(["", f"Нет артефактов: {result['missingCodeObjects']}."])
    return "\n".join(lines) + "\n"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifacts", type=Path)
    parser.add_argument("profile", type=Path)
    parser.add_argument("--output", type=Path, required=True, help="Prefix for .json and .md results")
    parser.add_argument("--top", type=int, default=6)
    args = parser.parse_args()
    result = analyze(args.artifacts, args.profile, args.top)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.with_suffix(".json").write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    args.output.with_suffix(".md").write_text(markdown(result))
    print(f"{result['listedSelfSamples']} self; {result['generatedSelfSamples']} generated; "
          f"JSON/MD: {args.output}")


if __name__ == "__main__":
    main()
