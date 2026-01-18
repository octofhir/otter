#!/bin/bash
set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(dirname "$SCRIPT_DIR")"

# Parse arguments
VERSION=""
DRY_RUN=true

while [[ $# -gt 0 ]]; do
    case $1 in
        --version)
            VERSION="$2"
            shift 2
            ;;
        --publish)
            DRY_RUN=false
            shift
            ;;
        --dry-run)
            DRY_RUN=true
            shift
            ;;
        -h|--help)
            echo "Usage: $0 [--version VERSION] [--publish] [--dry-run]"
            echo ""
            echo "Options:"
            echo "  --version VERSION  Override version (default: from Cargo.toml)"
            echo "  --publish          Actually publish to npm (default: dry-run)"
            echo "  --dry-run          Only show what would be published (default)"
            exit 0
            ;;
        *)
            echo "Unknown option: $1"
            echo "Use --help for usage"
            exit 1
            ;;
    esac
done

# Get version from Cargo.toml if not provided
if [ -z "$VERSION" ]; then
    VERSION=$(grep '^version' "$ROOT_DIR/crates/otterjs/Cargo.toml" | head -1 | sed 's/.*"\(.*\)"/\1/')
fi

echo "Publishing otter-types@$VERSION"
echo ""

# Prepare package
rm -rf "$ROOT_DIR/packages/otter-types"
mkdir -p "$ROOT_DIR/packages/otter-types"

# Copy types
cp "$ROOT_DIR/crates/otter-pm/src/types/otter/index.d.ts" "$ROOT_DIR/packages/otter-types/"
cp "$ROOT_DIR/crates/otter-pm/src/types/otter/globals.d.ts" "$ROOT_DIR/packages/otter-types/"
cp "$ROOT_DIR/crates/otter-pm/src/types/otter/sql.d.ts" "$ROOT_DIR/packages/otter-types/"
cp "$ROOT_DIR/crates/otter-pm/src/types/otter/serve.d.ts" "$ROOT_DIR/packages/otter-types/"

# Generate package.json
cat > "$ROOT_DIR/packages/otter-types/package.json" << EOF
{
  "name": "otter-types",
  "version": "$VERSION",
  "description": "TypeScript definitions for Otter runtime",
  "types": "./index.d.ts",
  "license": "MIT",
  "repository": {
    "type": "git",
    "url": "https://github.com/octofhir/otter",
    "directory": "packages/otter-types"
  },
  "keywords": ["otter", "javascript", "typescript", "types", "runtime"],
  "dependencies": {
    "@types/node": "*"
  }
}
EOF

# Generate README
cat > "$ROOT_DIR/packages/otter-types/README.md" << 'EOF'
# otter-types

TypeScript definitions for [Otter](https://github.com/octofhir/otter) runtime.

## Installation

```bash
npm install otter-types
```

This package depends on `@types/node` for Web API and Node.js type definitions.

## Usage

Types are automatically available when you install this package. For best results, use this `tsconfig.json`:

```json
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "bundler",
    "lib": ["ES2022"],
    "types": ["otter-types"],
    "skipLibCheck": true
  }
}
```

## What's included

- **globals.d.ts** - CommonJS support (`require`, `module`, `exports`, `__dirname`, `__filename`)
- **sql.d.ts** - SQL and KV store APIs
- **serve.d.ts** - HTTP server APIs (`Otter.serve()`)

Web APIs (fetch, URL, TextEncoder, etc.) and Node.js APIs (fs, path, etc.) come from `@types/node`.
EOF

cd "$ROOT_DIR/packages/otter-types"

# Show package contents
echo "=== Package contents ==="
ls -la
echo ""

if [ "$DRY_RUN" = true ]; then
    echo "=== Dry run (use --publish to actually publish) ==="
    npm publish --access public --dry-run
    echo ""
    echo "To publish for real, run:"
    echo "  $0 --publish"
else
    echo "=== Publishing to npm ==="
    npm publish --access public
    echo ""
    echo "Published otter-types@$VERSION"
fi
