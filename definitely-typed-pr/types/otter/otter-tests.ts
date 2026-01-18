// Type definitions for otter
// Project: https://github.com/octofhir/otter
// Definitions: https://github.com/DefinitelyTyped/DefinitelyTyped

// This is a shim that loads otter-types
/// <reference types="otter-types" />

// Basic tests to verify types are loaded
const mod: NodeModule = module;
const dir: string = __dirname;
const file: string = __filename;

// CommonJS require
const fs = require("fs");
