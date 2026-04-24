#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
binary_path="${1:-"$repo_root/target/release/otter"}"
entitlements_path="$repo_root/release/macos-jit-entitlements.plist"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "macOS release signing must run on Darwin" >&2
  exit 1
fi

if [[ ! -f "$binary_path" ]]; then
  echo "binary not found: $binary_path" >&2
  exit 1
fi

if [[ -z "${CODESIGN_IDENTITY:-}" ]]; then
  echo "set CODESIGN_IDENTITY to a Developer ID Application identity" >&2
  echo 'example: CODESIGN_IDENTITY="Developer ID Application: Example Inc (TEAMID)"' >&2
  exit 1
fi

codesign --force --timestamp --options runtime \
  --entitlements "$entitlements_path" \
  --sign "$CODESIGN_IDENTITY" \
  "$binary_path"

codesign --verify --strict --verbose=2 "$binary_path"

if ! codesign -d --entitlements - "$binary_path" 2>/dev/null \
  | grep -q "com.apple.security.cs.allow-jit"; then
  echo "signed binary is missing com.apple.security.cs.allow-jit" >&2
  exit 1
fi
