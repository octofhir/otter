# otterjs

A fast TypeScript/JavaScript runtime CLI.

## Installation

```bash
cargo install otterjs
```

## Usage

```bash
# Run a script directly
otter script.ts
otter script.js

# Or with the run command
otter run script.ts
otter run script.ts --watch
otter run script.ts --timeout 5000
```

## Commands

| Command | Description |
|---------|-------------|
| `run <file>` | Execute a TypeScript or JavaScript file |
| `check <file>` | Type check TypeScript files |
| `test` | Run tests |
| `repl` | Start interactive REPL |
| `install` | Install dependencies from package.json |
| `add <package>` | Add a dependency |
| `remove <package>` | Remove a dependency |
| `init` | Initialize a new project |
| `info` | Show runtime information |

## Examples

```bash
# Run with watch mode
otter run src/index.ts --watch

# Type check
otter check src/**/*.ts

# Run tests
otter test

# Start REPL
otter repl

# Package management
otter init
otter add lodash
otter install
otter remove lodash
```

## License

MIT
