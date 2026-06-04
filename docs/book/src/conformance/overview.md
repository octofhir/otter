# ECMAScript Conformance

Otter tracks its ECMAScript conformance against the official
[tc39/test262](https://github.com/tc39/test262) suite. Every
full-corpus run produces a merged JSON baseline and an interactive
dashboard.

## Interactive dashboard

The dashboard shows the headline pass rate, a drill-down tree of every
test262 group with per-group progress bars, and the failing tests for
each section with their failure reasons (each test links to its source
in the pinned test262 commit):

**[Open the conformance dashboard](./index.html)**

## Regenerating

The dashboard is a single self-contained HTML file generated from the
latest merged baseline:

```sh
# Run the full corpus in crash-safe batches; writes
# test262_results/latest.json and the dashboard automatically.
bash scripts/test262-full-run.sh

# Re-render the dashboard from an existing baseline and refresh the
# copy embedded in this book.
just test262-site
```

`just test262-site` runs `otter-test262 site` over
`test262_results/latest.json` and copies the output to
`docs/book/src/conformance/index.html`, which mdBook ships verbatim
into the published site.

See `ES_CONFORMANCE.md` at the repository root for the measured
baseline history and per-fix deltas.
