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
