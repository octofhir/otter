#!/usr/bin/env python3
"""Function-level JS coverage of a page load in headless Chrome (CDP).

usage: chrome_coverage.py URL [settle_seconds]
Reports, over every script the page ran: functions defined, functions called
at least once by `load` + settle, and the share of function source bytes that
belong to called functions (each function's own bytes, nested ones excluded).
This is the fraction of code a lazily compiling engine has to compile.
"""
import importlib.util, json, os, shutil, subprocess, sys, tempfile, time, urllib.request

spec = importlib.util.spec_from_file_location(
    "chrome_load", os.path.join(os.path.dirname(__file__), "..", "..", "..", "Otlyra", "tools", "chrome-load.py"))
src = open(spec.origin).read().replace("\nmain()\n", "\n")
ns = {}
exec(compile(src, spec.origin, "exec"), ns)
Client, CHROME = ns["Client"], ns["CHROME"]


def own_bytes(functions):
    """Own byte size of each function: its range minus directly nested ones."""
    ranges = [(f["ranges"][0]["startOffset"], f["ranges"][0]["endOffset"], f) for f in functions]
    ranges.sort(key=lambda r: (r[0], -r[1]))
    out = []
    stack = []
    sizes = {}
    for start, end, f in ranges:
        while stack and stack[-1][1] <= start:
            stack.pop()
        if stack:
            sizes[id(stack[-1][2])] -= (end - start)
        sizes[id(f)] = end - start
        stack.append((start, end, f))
    for start, end, f in ranges:
        out.append((f, max(0, sizes[id(f)])))
    return out


def main():
    url = sys.argv[1]
    settle = float(sys.argv[2]) if len(sys.argv) > 2 else 2.0
    port = 9334
    profile = tempfile.mkdtemp(prefix="chrome-cov-")
    proc = subprocess.Popen([CHROME, "--headless=new", f"--remote-debugging-port={port}",
                             f"--user-data-dir={profile}", "--window-size=1280,900",
                             "--no-first-run", "--no-default-browser-check", "about:blank"],
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    try:
        for _ in range(100):
            try:
                targets = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json"))
                page = next(t for t in targets if t["type"] == "page")
                break
            except Exception:
                time.sleep(0.1)
        c = Client(page["webSocketDebuggerUrl"])
        c.call("Page.enable")
        c.call("Profiler.enable")
        c.call("Profiler.startPreciseCoverage", {"callCount": True, "detailed": False})
        c.call("Page.navigate", {"url": url})
        c.wait_event("Page.loadEventFired", 60)
        time.sleep(settle)
        cov = c.call("Profiler.takePreciseCoverage")["result"]
        total_f = called_f = 0
        total_b = called_b = 0
        for script in cov:
            if not script["url"] or script["url"].startswith("chrome"):
                continue
            for f, size in own_bytes(script["functions"]):
                if not f["functionName"] and f["ranges"][0]["startOffset"] == 0:
                    continue  # the script's top level
                total_f += 1
                total_b += size
                if f["ranges"][0]["count"] > 0:
                    called_f += 1
                    called_b += size
        print(json.dumps({"url": url, "scripts": len(cov), "functions": total_f, "called": called_f,
                          "called_share": round(called_f / max(1, total_f), 3),
                          "function_bytes": total_b, "called_bytes": called_b,
                          "called_bytes_share": round(called_b / max(1, total_b), 3)}))
    finally:
        proc.terminate(); proc.wait()
        shutil.rmtree(profile, ignore_errors=True)


main()
