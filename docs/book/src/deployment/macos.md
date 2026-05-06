# macOS Deployment

Otter release automation can sign macOS artifacts with a hardened runtime. Any
future JIT-enabled macOS ARM64 release must include the JIT entitlement.

Manual signing:

```bash
codesign --force --options runtime \
  --entitlements release/macos-jit-entitlements.plist \
  --sign "Developer ID Application: <Team Name>" \
  target/release/otter
```

Release helper:

```bash
CODESIGN_IDENTITY="Developer ID Application: <Team Name>" \
  scripts/sign-macos-release.sh target/release/otter
```

Secrets used by release workflows:

- `APPLE_CERTIFICATE_P12_BASE64`
- `APPLE_CERTIFICATE_PASSWORD`
- `APPLE_CODESIGN_IDENTITY`
- `MACOS_KEYCHAIN_PASSWORD`

The required entitlement is:

```xml
<key>com.apple.security.cs.allow-jit</key>
<true/>
```

Run a JIT smoke test after signing when JIT is enabled in the release build.
