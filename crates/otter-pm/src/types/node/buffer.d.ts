/**
 * The `node:buffer` module provides a way to handle binary data directly.
 * @module node:buffer
 */
declare module "node:buffer" {
    /**
     * Supported buffer encodings.
     */
    export type BufferEncoding = "utf8" | "utf-8" | "base64" | "hex";

    /**
     * Buffer instance interface.
     */
    export interface Buffer extends Uint8Array {
        /**
         * Decodes the Buffer to a string according to the specified encoding.
         */
        toString(encoding?: BufferEncoding, start?: number, end?: number): string;

        /**
         * Returns a new Buffer that references the same memory as the original.
         */
        slice(start?: number, end?: number): Buffer;

        /**
         * Copies data from a region of this Buffer to a region in target.
         */
        copy(target: Buffer, targetStart?: number, sourceStart?: number, sourceEnd?: number): number;

        /**
         * Returns true if both this and other have exactly the same bytes.
         */
        equals(other: Buffer): boolean;

        /**
         * Compares this buffer with other.
         */
        compare(other: Buffer): -1 | 0 | 1;
    }

    /**
     * Buffer constructor interface.
     */
    export interface BufferConstructor {
        /**
         * Allocates a new Buffer of size bytes.
         */
        alloc(size: number, fill?: number | string, encoding?: BufferEncoding): Buffer;

        /**
         * Allocates a new Buffer using data.
         */
        from(data: string | number[] | Buffer | ArrayBuffer | Uint8Array, encoding?: BufferEncoding): Buffer;

        /**
         * Returns a new Buffer which is the result of concatenating all the Buffer instances together.
         */
        concat(list: Buffer[], totalLength?: number): Buffer;

        /**
         * Returns true if obj is a Buffer, false otherwise.
         */
        isBuffer(obj: unknown): obj is Buffer;

        /**
         * Returns the byte length of a string when encoded using encoding.
         */
        byteLength(string: string | Buffer, encoding?: BufferEncoding): number;

        readonly prototype: Buffer;
    }

    export const Buffer: BufferConstructor;
}

// Also support the 'buffer' module (without node: prefix)
declare module "buffer" {
    export * from "node:buffer";
}

// Add Buffer to global scope
declare global {
    const Buffer: import("node:buffer").BufferConstructor;
}
