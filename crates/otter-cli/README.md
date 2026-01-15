# otter-cli

A fast TypeScript/JavaScript runtime CLI.

## Overview

`otter-cli` provides the `otter` command-line tool for executing TypeScript and JavaScript files with the Otter runtime.

## Installation

```bash
cargo install otter-cli
```

## Usage

```bash
# Run a TypeScript file
otter run script.ts

# Run a JavaScript file
otter run script.js

# Install dependencies
otter install

# Initialize a new project
otter init
```

## Commands

| Command | Description |
|---------|-------------|
| `run <file>` | Execute a TypeScript or JavaScript file |
| `install` | Install npm dependencies |
| `init` | Initialize a new otter project |

## License

MIT
