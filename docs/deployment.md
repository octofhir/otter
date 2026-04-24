# Deployment Notes

## macOS ARM64 JIT Signing

Otter's baseline JIT allocates executable memory on macOS ARM64 with `MAP_JIT` and Darwin's per-thread JIT write-protect API. Unsigned local development builds work without extra setup, but any hardened-runtime binary distributed to users must be signed with the JIT entitlement.

Use the checked-in entitlement file when signing release binaries:

```bash
codesign --force --options runtime \
  --entitlements release/macos-jit-entitlements.plist \
  --sign "Developer ID Application: <Team Name>" \
  target/release/otter
```

The repository also includes a signing helper for release automation:

```bash
CODESIGN_IDENTITY="Developer ID Application: <Team Name>" \
  scripts/sign-macos-release.sh target/release/otter
```

The GitHub release workflows call this helper before packaging macOS artifacts when signing secrets are configured. Configure these repository secrets to enable signed macOS artifacts:

- `APPLE_CERTIFICATE_P12_BASE64`: base64-encoded Developer ID Application `.p12`.
- `APPLE_CERTIFICATE_PASSWORD`: password for the `.p12`.
- `APPLE_CODESIGN_IDENTITY`: exact `codesign` identity, for example `Developer ID Application: Example Inc (TEAMID)`.
- `MACOS_KEYCHAIN_PASSWORD`: optional temporary CI keychain password.

If these secrets are absent, the macOS release jobs emit a warning and package unsigned binaries instead of blocking the release. That keeps CI usable without an Apple Developer account, but those artifacts may show Gatekeeper warnings and are not the hardened-runtime JIT release path. Once the secrets are configured, signing is automatic and the helper fails the job if the signed binary does not contain `com.apple.security.cs.allow-jit`.

The required entitlement is:

```xml
<key>com.apple.security.cs.allow-jit</key>
<true/>
```

Release automation for macOS ARM64 must run a JIT smoke test after signing. A minimal check is a hot integer loop that triggers tier-up, for example:

```bash
printf '%s\n' \
  'function sum(n){let s=0;let i=0;while(i<n){s=(s+i)|0;i=i+1;}return s;}' \
  'let out=0; for (let i=0; i<120; i=i+1) out=sum(1000); out;' \
  >/tmp/otter-jit-smoke.js
OTTER_JIT_THRESHOLD=10 target/release/otter --dump-jit-stats run /tmp/otter-jit-smoke.js
```

The smoke test should verify the process exits successfully and the JIT telemetry reports at least one JIT entry.
