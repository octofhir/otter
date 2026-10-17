#!/usr/bin/env python3
"""Fetch the external scripts of real pages into benchmarks/fixtures/web-bundles/<site>/.

Usage: python3 -I scripts/dev/fetch_web_bundles.py github.com www.theverge.com ...
Writes NN-<name>.js in document order plus manifest.txt (order, url, bytes).
"""
import html, os, re, sys, urllib.request, urllib.parse

UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/141.0.0.0 Safari/537.36")
ROOT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..",
                    "benchmarks", "fixtures", "web-bundles")

def get(url):
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Accept": "*/*"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return r.read()

def main():
    for site in sys.argv[1:]:
        base = "https://" + site + "/"
        page = get(base).decode("utf-8", "replace")
        urls = []
        for m in re.finditer(r'<(script|link)\b([^>]*)>', page, re.I):
            tag, attrs = m.group(1).lower(), m.group(2)
            if tag == "link" and not re.search(r'rel="?modulepreload', attrs, re.I):
                continue
            src = re.search(r'\b(?:src|href)="([^"]+)"', attrs)
            if not src:
                continue
            u = urllib.parse.urljoin(base, html.unescape(src.group(1)))
            if u not in urls:
                urls.append(u)
        out = os.path.join(ROOT, site)
        os.makedirs(out, exist_ok=True)
        lines = []
        for i, u in enumerate(urls):
            name = os.path.basename(urllib.parse.urlparse(u).path) or "index.js"
            if not name.endswith((".js", ".mjs")):
                name += ".js"
            try:
                body = get(u)
            except Exception as e:  # noqa: BLE001
                print("skip", u, e)
                continue
            fn = "%02d-%s" % (i, name[:60])
            with open(os.path.join(out, fn), "wb") as f:
                f.write(body)
            lines.append("%s\t%s\t%d" % (fn, u, len(body)))
        with open(os.path.join(out, "manifest.txt"), "w") as f:
            f.write("\n".join(lines) + "\n")
        print(site, len(lines), "scripts", sum(int(l.split("\t")[2]) for l in lines), "bytes")

main()
