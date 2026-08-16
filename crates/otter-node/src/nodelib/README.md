# Vendored Node.js lib sources

Everything in this directory except `compat/` is copied verbatim from
[nodejs/node](https://github.com/nodejs/node) `lib/`, at the commit the
conformance corpus under `tests/node-compat/node` tracks (see
`scripts/fetch-node-tests.sh`). The sources are MIT-licensed; the license
text ships alongside them in `LICENSE`, and the per-file copyright headers
are preserved.

Rules:

- Vendored files are never edited. Behavioral divergence is fixed in
  `compat/` or in the engine, so a refresh stays a plain copy from the
  corpus checkout.
- `compat/` is original code: narrow stand-ins for the `internal/*`
  dependencies whose Node implementations sit on native bindings.
