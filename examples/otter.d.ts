// Otter Runtime Type Definitions
//
// The `Otter` global holds what the runtime installs on it — today
// `Otter.serve` and `Otter.XML`. Those declarations are published, so they
// are referenced here rather than written a second time.
/// <reference path="../packages/otter-types/serve.d.ts" />
/// <reference path="../packages/otter-types/xml.d.ts" />

// Console API
interface Console {
  log(...args: unknown[]): void;
  info(...args: unknown[]): void;
  warn(...args: unknown[]): void;
  error(...args: unknown[]): void;
  debug(...args: unknown[]): void;
  trace(...args: unknown[]): void;
  assert(condition?: boolean, ...args: unknown[]): void;
  clear(): void;
  count(label?: string): void;
  countReset(label?: string): void;
  group(...args: unknown[]): void;
  groupCollapsed(...args: unknown[]): void;
  groupEnd(): void;
  table(data: unknown, columns?: string[]): void;
  time(label?: string): void;
  timeEnd(label?: string): void;
  timeLog(label?: string, ...args: unknown[]): void;
}

declare var console: Console;

// Timers
declare function setTimeout(callback: (...args: unknown[]) => void, ms?: number, ...args: unknown[]): number;
declare function clearTimeout(id: number): void;
declare function setInterval(callback: (...args: unknown[]) => void, ms?: number, ...args: unknown[]): number;
declare function clearInterval(id: number): void;

// Text encoding/decoding
declare class TextEncoder {
  readonly encoding: string;
  encode(input?: string): Uint8Array;
  encodeInto(input: string, dest: Uint8Array): { read: number; written: number };
}

declare class TextDecoder {
  readonly encoding: string;
  readonly fatal: boolean;
  readonly ignoreBOM: boolean;
  constructor(encoding?: string, options?: { fatal?: boolean; ignoreBOM?: boolean });
  decode(input?: ArrayBuffer | ArrayBufferView, options?: { stream?: boolean }): string;
}

// URL API
declare class URL {
  constructor(url: string | URL, base?: string | URL);
  hash: string;
  host: string;
  hostname: string;
  href: string;
  readonly origin: string;
  password: string;
  pathname: string;
  port: string;
  protocol: string;
  search: string;
  readonly searchParams: URLSearchParams;
  username: string;
  toString(): string;
  toJSON(): string;
}

declare class URLSearchParams {
  constructor(init?: string | string[][] | Record<string, string> | URLSearchParams);
  append(name: string, value: string): void;
  delete(name: string): void;
  get(name: string): string | null;
  getAll(name: string): string[];
  has(name: string): boolean;
  set(name: string, value: string): void;
  sort(): void;
  toString(): string;
  forEach(callback: (value: string, key: string, parent: URLSearchParams) => void): void;
  keys(): IterableIterator<string>;
  values(): IterableIterator<string>;
  entries(): IterableIterator<[string, string]>;
  [Symbol.iterator](): IterableIterator<[string, string]>;
}

// Otter is available globally; what it holds is declared by the
// referenced files at the top of this one.
declare var Otter: typeof Otter;
