# Otter Tooling Roadmap

This document tracks tooling development for Otter. Tooling is **non-blocking** for VM development and is developed separately.

## Status Overview

| Tool | Status | Priority | Depends On |/
|------|--------|----------|------------|
| Package Manager | Basic (`otter-pm`) | Medium | - |
| Bundler | Not started | Low | Phase 4 Runtime |
| Test Runner | Not started | Medium | Phase 4 Runtime |
| Debugger | Not started | Medium | Source Maps (Phase 5) |

---

## 1. Package Manager (`otter-pm`)

**Location**: `crates/otter-pm/`

**Current State**: Basic implementation exists.

### Features

| Feature | Status | Notes |
|---------|--------|-------|
| npm registry fetch | Done | Basic package download |
| Dependency resolution | Partial | Needs version conflict handling |
| Lockfile support | Not started | `otter.lock` format TBD |
| Workspaces | Not started | Monorepo support |
| Cache management | Partial | Local cache exists |

### Roadmap

1. **v0.1** (current): Basic install from npm
2. **v0.2**: Lockfile generation and resolution
3. **v0.3**: Workspace support
4. **v1.0**: Feature parity with npm for common use cases

---

## 2. Bundler

**Location**: `crates/otter-bundler/` (to be created)

**Status**: Not started

### Planned Features

| Feature | Priority | Notes |
|---------|----------|-------|
| ES module bundling | High | Single output file |
| Tree shaking | High | Dead code elimination |
| Code splitting | Medium | Multiple output chunks |
| Minification | Medium | Variable renaming, whitespace |
| Source maps | High | For debugging bundled code |

### Architecture

```
Source files → oxc parser → Module graph → Tree shaking → Code generation → Output
```

**Key decisions**:

- Reuse oxc parser (already a dependency)
- No need for full Rollup/webpack compatibility
- Focus on ES modules (not CommonJS bundling)

### Dependencies

- Requires stable module resolution (Phase 4 Runtime)
- Requires oxc AST manipulation utilities

---

## 3. Test Runner

**Location**: `crates/otter-test/` (to be created)

**Status**: Not started

### Planned Features

| Feature | Priority | Notes |
|---------|----------|-------|
| Test discovery | High | `*.test.ts`, `*.spec.ts` patterns |
| `describe`/`it`/`expect` API | High | Jest-compatible |
| Async test support | High | Promise-based tests |
| Test isolation | Medium | Fresh context per test |
| Watch mode | Low | Re-run on file changes |
| Coverage | Low | Requires instrumentation |

### API Design

```typescript
// Jest-compatible API
describe('MyModule', () => {
  it('should do something', () => {
    expect(add(1, 2)).toBe(3);
  });

  it('should handle async', async () => {
    const result = await fetchData();
    expect(result).toBeDefined();
  });
});
```

### Implementation

Test runner builtins in runtime extensions (planned under `crates/otter-engine/src/`):

- `describe(name, fn)` - Test suite
- `it(name, fn)` / `test(name, fn)` - Test case
- `expect(value)` - Assertion builder
- `beforeEach`, `afterEach`, `beforeAll`, `afterAll` - Hooks

### Dependencies

- Requires event loop for async tests (Phase 4 Runtime)
- Requires stable Promise implementation

---

## 4. Debugger

**Location**: TBD

**Status**: Not started

### Planned Features

| Feature | Priority | Notes |
|---------|----------|-------|
| Breakpoints | High | Line and conditional |
| Step execution | High | Step in/over/out |
| Variable inspection | High | Scope and closure variables |
| Call stack | High | With source locations |
| Chrome DevTools Protocol | Medium | For IDE integration |

### Architecture Options

**Option A**: Built-in debug server

- Otter includes CDP server
- Connect via Chrome DevTools or VS Code

**Option B**: Debug adapter protocol (DAP)

- Implement DAP for VS Code
- More portable across editors

### Dependencies

- Requires source maps (Phase 5)
- Requires interpreter debug hooks

---

## 5. CLI Integration

All tools integrate via `otter` CLI:

```bash
# Package manager
otter add <package>          # Add dependency
otter install                # Install from lockfile
otter update                 # Update dependencies

# Bundler
otter build                  # Bundle for production
otter build --watch          # Watch mode

# Test runner
otter test                   # Run all tests
otter test --watch           # Watch mode
otter test path/to/file      # Run specific file

# Debugger
otter debug script.ts        # Start debug session
```

---

## Timeline

Tooling development happens **after** VM Phase 4 (Runtime Integration) is complete.

| Phase | Tooling Work |
|-------|--------------|
| VM Phase 0-3 | No tooling work |
| VM Phase 4 | Package manager v0.2 (lockfiles) |
| VM Phase 5 | Test runner v0.1, Bundler v0.1 |
| Post-VM | Debugger, tooling polish |

---

## Non-Goals

- **Full webpack/Rollup compatibility**: Focus on common patterns
- **Plugin ecosystem**: Keep it simple, no plugin API initially
- **Build system**: Not a Make/Gradle replacement
- **Monorepo orchestration**: Use existing tools (Turborepo, Nx)
